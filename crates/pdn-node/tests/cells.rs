//! The cells service end to end: creating a cell, listing cells and their
//! members, the invite and join dialogue between in-process runtimes — the
//! invite passed as a value — with the refusals of its verify-and-burn each
//! probed for no observable state beside its allowed counterpart, the
//! membership acts each beside the same act refused, and the records
//! members place, edit and read.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use data_layer::{
    cell_inviter_ticket_kind, cell_ticket_kind, CellStore, MemberDevice, MemberState, RecordKey,
    Verdict,
};
use pdn_node::{
    ActRefusal, ActRefused, CellAct, CellInvite, CellMember, CellsService as _,
    ConnectionsService as _, IdentityService as _, JoinRefused, RecordPlacedOnce, Runtime,
    SpawnOptions, UnknownCell, UnknownIdentity, UnknownRecord, UnsupportedCellInviteVersion,
    WrongRecordKind,
};
use pdn_types::{CellId, PdnId, RecordId, RecordKind, RecordRef};
use test_utils::{eventually, ids};

mod common;
use common::{link_patiently, link_probe, memory_runtime};

fn member(id: PdnId, owner: bool) -> CellMember {
    CellMember { id, owner }
}

/// Whether `holder` on `runtime` comes to list exactly `want` as the
/// members of `cell`.
async fn lists_members(
    runtime: &Runtime,
    holder: PdnId,
    cell: CellId,
    mut want: Vec<CellMember>,
) -> Result<bool> {
    want.sort();
    eventually(|| async {
        Ok(runtime.cells().members(holder, cell).await.ok() == Some(want.clone()))
    })
    .await
}

fn is<E: std::error::Error + Send + Sync + 'static>(err: &anyhow::Error) -> bool {
    err.downcast_ref::<E>().is_some()
}

/// A created cell is listed with its creator as its one member and owner,
/// and a second cell of the same identity is a second id. Denied: a create
/// for an identity the runtime does not host, and the members of a cell
/// asked by a co-located identity that is no member of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_created_cell_is_listed_with_its_creator_as_owner() -> Result<()> {
    let phone = memory_runtime().await?;
    let alice = phone.identity().create().await?;
    let family = phone.cells().create(alice).await?;
    let wedding = phone.cells().create(alice).await?;
    assert_ne!(family, wedding);
    let mut cells: Vec<CellId> = phone
        .cells()
        .list(alice)
        .await?
        .into_iter()
        .map(|info| info.id)
        .collect();
    let mut expected = vec![family, wedding];
    cells.sort();
    expected.sort();
    assert_eq!(cells, expected);
    for cell in [family, wedding] {
        assert_eq!(
            phone.cells().members(alice, cell).await?,
            vec![member(alice, true)]
        );
    }

    // Denied: an identity the runtime does not host.
    let refused = phone.cells().create(ids::DAVE).await;
    assert!(refused.is_err_and(|err| is::<UnknownIdentity>(&err)));
    // Denied: a co-located identity that is no member.
    let erin = phone.identity().create().await?;
    let asked = phone.cells().members(erin, family).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));
    assert!(phone.cells().list(erin).await?.is_empty());

    phone.shutdown().await?;
    Ok(())
}

/// A newcomer joins with an invite, catches up, and both devices list it
/// among the members, a plain one. Denied: the same secret presented again,
/// by a third identity, is refused, and the cell's members are exactly what
/// the first join left.
#[tokio::test(flavor = "multi_thread")]
async fn a_newcomer_joins_and_a_replayed_secret_is_refused() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let cell = alice_phone.cells().create(alice).await?;
    let invite = alice_phone.cells().invite(alice, cell, None).await?;

    assert_eq!(bob_phone.cells().join(bob, invite.clone()).await?, cell);
    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_members(&bob_phone, bob, cell, both.clone()).await?);
    assert!(lists_members(&alice_phone, alice, cell, both.clone()).await?);
    assert!(bob_phone
        .cells()
        .list(bob)
        .await?
        .iter()
        .any(|info| info.id == cell));

    // Denied: the burned secret, again.
    let replayed = carol_phone.cells().join(carol, invite).await;
    assert!(replayed.is_err_and(|err| is::<JoinRefused>(&err)));
    assert_eq!(alice_phone.cells().members(alice, cell).await?, {
        let mut both = both;
        both.sort();
        both
    });
    assert!(carol_phone.cells().list(carol).await?.is_empty());

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A member invited by the creator invites in turn, and the creator's
/// device lists the third member with no act of its own.
#[tokio::test(flavor = "multi_thread")]
async fn an_invited_member_invites_in_turn() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let cell = alice_phone.cells().create(alice).await?;
    let from_alice = alice_phone.cells().invite(alice, cell, None).await?;
    bob_phone.cells().join(bob, from_alice).await?;
    let from_bob = bob_phone.cells().invite(bob, cell, None).await?;
    carol_phone.cells().join(carol, from_bob).await?;

    let all = vec![
        member(alice, true),
        member(bob, false),
        member(carol, false),
    ];
    assert!(
        lists_members(&alice_phone, alice, cell, all).await?,
        "the creator's device did not list the member its member invited"
    );

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// The pending invite's real secret joins after a secret never minted and
/// an expired invite were refused with no observable state; an invite of a
/// version the runtime does not know is refused before any dial.
#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_secret_burns_nothing() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = alice_phone.cells().create(alice).await?;
    let invite = alice_phone.cells().invite(alice, cell, None).await?;

    let wrong = CellInvite {
        secret: [0x5a; 32],
        ..invite.clone()
    };
    let refused = bob_phone.cells().join(bob, wrong).await;
    assert!(refused.is_err_and(|err| is::<JoinRefused>(&err)));
    assert_eq!(
        alice_phone.cells().members(alice, cell).await?,
        vec![member(alice, true)]
    );
    assert!(bob_phone.cells().list(bob).await?.is_empty());
    let expired = alice_phone
        .cells()
        .invite(alice, cell, Some(std::time::Duration::from_millis(1)))
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let refused = bob_phone.cells().join(bob, expired).await;
    assert!(refused.is_err_and(|err| is::<JoinRefused>(&err)));
    assert!(bob_phone.cells().list(bob).await?.is_empty());
    let unknown_version = CellInvite {
        version: 9,
        ..invite.clone()
    };
    let refused = bob_phone.cells().join(bob, unknown_version).await;
    assert!(refused.is_err_and(|err| is::<UnsupportedCellInviteVersion>(&err)));

    bob_phone.cells().join(bob, invite).await?;
    assert!(
        lists_members(
            &alice_phone,
            alice,
            cell,
            vec![member(alice, true), member(bob, false)]
        )
        .await?
    );

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Two identities of one node invite and join with no other node
/// reachable, each listing the other. Denied: the secret presented again
/// inside the process is refused.
#[tokio::test(flavor = "multi_thread")]
async fn two_identities_of_one_node_invite_and_join() -> Result<()> {
    let tablet = memory_runtime().await?;
    let leisure = tablet.identity().create().await?;
    let work = tablet.identity().create().await?;
    let erin = tablet.identity().create().await?;
    let cell = tablet.cells().create(leisure).await?;
    let invite = tablet.cells().invite(leisure, cell, None).await?;
    tablet.cells().join(work, invite.clone()).await?;

    let both = vec![member(leisure, true), member(work, false)];
    for holder in [leisure, work] {
        assert!(lists_members(&tablet, holder, cell, both.clone()).await?);
    }
    // Denied: the burned secret, by a third co-located identity.
    let replayed = tablet.cells().join(erin, invite).await;
    assert!(replayed.is_err_and(|err| is::<JoinRefused>(&err)));
    assert!(tablet.cells().list(erin).await?.is_empty());

    tablet.shutdown().await?;
    Ok(())
}

/// Two members hosted on one node write and converge with no other node
/// reachable, listed under the node's one id with an author each, every
/// entry reading as the member whose author signed it; one leaves, and the
/// other keeps its copy, reading what the first wrote and writing on.
/// Denied: a co-located identity that is no member, and the member that
/// left, read nothing.
#[tokio::test(flavor = "multi_thread")]
async fn two_members_on_one_node_write_converge_and_part_each_as_itself() -> Result<()> {
    let tablet = memory_runtime().await?;
    let leisure = tablet.identity().create().await?;
    let work = tablet.identity().create().await?;
    let erin = tablet.identity().create().await?;
    let cell = tablet.cells().create(leisure).await?;
    let invite = tablet.cells().invite(leisure, cell, None).await?;
    tablet.cells().join(work, invite).await?;

    let cells = tablet.cells();
    let claim = cells
        .put_record(leisure, cell, RecordKind::Claim, b"leisure's claim")
        .await?;
    let scan = cells
        .put_record(work, cell, RecordKind::ImmutableDocument, b"work's scan")
        .await?;
    let note = cells
        .put_record(leisure, cell, RecordKind::MergeableDocument, b"milk")
        .await?;
    let first = vec![(leisure, 1, b"milk".to_vec())];
    assert!(reads_ops(&tablet, work, cell, note, first).await?);
    cells.append_op(work, cell, note, b"eggs").await?;
    let edited = vec![(leisure, 1, b"milk".to_vec()), (work, 1, b"eggs".to_vec())];
    for holder in [leisure, work] {
        assert!(reads(&tablet, holder, cell, claim, b"leisure's claim").await?);
        assert!(reads(&tablet, holder, cell, scan, b"work's scan").await?);
        assert!(reads_ops(&tablet, holder, cell, note, edited.clone()).await?);
    }
    let membership = tablet.cell_membership_for_test(work, cell).await?;
    let device_of = |id: PdnId| -> Vec<MemberDevice> {
        membership
            .member(&id)
            .map(|member| member.devices.iter().copied().collect())
            .unwrap_or_default()
    };
    let (leisures, works) = (device_of(leisure), device_of(work));
    let ([leisures], [works]) = (leisures.as_slice(), works.as_slice()) else {
        anyhow::bail!("a member on one node listed {leisures:?} and {works:?}");
    };
    assert_eq!(leisures.node, works.node);
    assert_ne!(leisures.author, works.author);
    for op in cells.read_ops(leisure, cell, note).await? {
        let signer = if op.id.writer == leisure {
            leisures.author
        } else {
            works.author
        };
        assert_eq!(
            op.id.author, signer,
            "an operation read under another author"
        );
    }
    // Denied (a co-located non-member).
    let asked = cells.read(erin, cell, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));

    cells.act(work, cell, CellAct::Leave).await?;
    assert!(cells.list(work).await?.is_empty());
    assert_eq!(tablet.cell_holdings_for_test(work).await?, [(cell, false)]);
    // Denied: the member that left.
    let asked = cells.read(work, cell, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));
    assert!(lists_members(&tablet, leisure, cell, vec![member(leisure, true)]).await?);
    assert!(reads(&tablet, leisure, cell, scan, b"work's scan").await?);
    let later = cells
        .put_record(leisure, cell, RecordKind::Claim, b"after the leave")
        .await?;
    assert!(reads(&tablet, leisure, cell, later, b"after the leave").await?);

    tablet.shutdown().await?;
    Ok(())
}

/// A newcomer whose join lost the inviter's reply, after its joined event
/// was written, joins through a second invite that writes no second joined
/// event, and catches up.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_whose_join_lost_the_reply_joins_through_a_second_invite() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = alice_phone.cells().create(alice).await?;

    alice_phone.drop_next_join_reply_for_test().await;
    let first = alice_phone.cells().invite(alice, cell, None).await?;
    let lost = bob_phone.cells().join(bob, first).await;
    assert!(lost.is_err_and(|err| is::<JoinRefused>(&err)));
    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_members(&alice_phone, alice, cell, both.clone()).await?);
    assert!(bob_phone.cells().list(bob).await?.is_empty());

    let second = alice_phone.cells().invite(alice, cell, None).await?;
    bob_phone.cells().join(bob, second).await?;
    assert!(lists_members(&bob_phone, bob, cell, both).await?);
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        let membership = runtime.cell_membership_for_test(holder, cell).await?;
        assert_eq!(
            membership.member(&bob).map(data_layer::Member::run),
            Some(1),
            "the second invite wrote a second joined event"
        );
    }

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A join whose `join` future is dropped during its catch-up, once both
/// tickets and the directory's entry are recorded, ends caught up all the
/// same: the newcomer lists the cell and its members and reads what was
/// placed before and after, by the one joined event. Denied: the invite
/// presented again.
#[tokio::test(flavor = "multi_thread")]
async fn a_join_whose_future_is_dropped_during_its_catch_up_is_finished() -> Result<()> {
    let alice_phone = memory_runtime().await?;
    let bob_phone = Arc::new(memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = alice_phone.cells().create(alice).await?;
    let before = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"before")
        .await?;
    let invite = alice_phone.cells().invite(alice, cell, None).await?;

    let pause = bob_phone.pause_next_join_catch_up().await;
    let joining = {
        let (bob_phone, invite) = (Arc::clone(&bob_phone), invite.clone());
        tokio::spawn(async move { bob_phone.cells().join(bob, invite).await })
    };
    pause.wait_until_reached().await;
    joining.abort();
    assert!(joining.await.unwrap_err().is_cancelled());
    pause.release();

    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_cell(&bob_phone, bob, cell, true).await?);
    assert!(lists_members(&bob_phone, bob, cell, both).await?);
    assert!(reads(&bob_phone, bob, cell, before, b"before").await?);
    let after = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"after")
        .await?;
    assert!(reads(&bob_phone, bob, cell, after, b"after").await?);
    let membership = bob_phone.cell_membership_for_test(bob, cell).await?;
    assert_eq!(
        membership.member(&bob).map(data_layer::Member::run),
        Some(1)
    );
    // Denied: the burned secret, again.
    let replayed = bob_phone.cells().join(bob, invite).await;
    assert!(replayed.is_err_and(|err| is::<JoinRefused>(&err)));

    alice_phone.shutdown().await?;
    bob_phone.shutdown().await?;
    Ok(())
}

/// A join cut by a restart during its catch-up, once both tickets and the
/// directory's entry are recorded, is finished by the armer after the
/// restart: the newcomer lists the cell and its members and reads what was
/// placed before the join and while it was down, by the one joined event.
/// Denied: the invite presented again.
#[tokio::test(flavor = "multi_thread")]
async fn a_join_cut_by_a_restart_during_its_catch_up_is_finished_by_the_armer() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let alice_phone = memory_runtime().await?;
    let bob_tablet = Arc::new(runtime_on(dir.path()).await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_tablet.identity().create().await?;
    let cell = alice_phone.cells().create(alice).await?;
    let before = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"before")
        .await?;
    let invite = alice_phone.cells().invite(alice, cell, None).await?;

    let pause = bob_tablet.pause_next_join_catch_up().await;
    let joining = {
        let (bob_tablet, invite) = (Arc::clone(&bob_tablet), invite.clone());
        tokio::spawn(async move { bob_tablet.cells().join(bob, invite).await })
    };
    pause.wait_until_reached().await;
    // The process ends: the `join` future with it, then the node.
    joining.abort();
    assert!(joining.await.unwrap_err().is_cancelled());
    bob_tablet.shutdown().await?;
    drop(bob_tablet);

    let meanwhile = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"meanwhile")
        .await?;
    let bob_tablet = runtime_on(dir.path()).await?;
    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_cell(&bob_tablet, bob, cell, true).await?);
    assert!(lists_members(&bob_tablet, bob, cell, both).await?);
    for (record, said) in [(before, &b"before"[..]), (meanwhile, &b"meanwhile"[..])] {
        assert!(
            reads(&bob_tablet, bob, cell, record, said).await?,
            "the armer did not catch the cell up after the restart"
        );
    }
    let membership = bob_tablet.cell_membership_for_test(bob, cell).await?;
    assert_eq!(
        membership.member(&bob).map(data_layer::Member::run),
        Some(1)
    );
    // Denied: the burned secret, again.
    let replayed = bob_tablet.cells().join(bob, invite).await;
    assert!(replayed.is_err_and(|err| is::<JoinRefused>(&err)));

    for runtime in [alice_phone, bob_tablet] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A join whose inviter goes out of reach for its whole catch-up fails with
/// the catch-up timeout and leaves both tickets and the directory's entry
/// recorded; once the inviter is reachable again the newcomer's cell pass
/// catches it up, by the one joined event. Denied: the invite presented
/// again. The inviter refusing every cell session stands in for the
/// connection gone, since the dialogue runs on its own protocol; the
/// catch-up's whole budget passes.
#[tokio::test(flavor = "multi_thread")]
async fn a_join_whose_inviter_drops_out_during_its_catch_up_is_finished_later() -> Result<()> {
    let alice_phone = memory_runtime().await?;
    let bob_phone = Runtime::spawn(SpawnOptions {
        cell_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    })
    .await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = alice_phone.cells().create(alice).await?;
    let before = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"before")
        .await?;
    let invite = alice_phone.cells().invite(alice, cell, None).await?;

    alice_phone
        .refuse_cell_sessions_for_test(alice, true)
        .await?;
    let cut = bob_phone.cells().join(bob, invite.clone()).await;
    assert!(cut.is_err_and(|err| is::<data_layer::CatchUpTimeout>(&err)));
    assert!(lists_cell(&bob_phone, bob, cell, true).await?);
    alice_phone
        .refuse_cell_sessions_for_test(alice, false)
        .await?;

    let both = vec![member(alice, true), member(bob, false)];
    assert!(
        lists_members(&bob_phone, bob, cell, both).await?,
        "the cell pass did not catch the cut join up"
    );
    assert!(reads(&bob_phone, bob, cell, before, b"before").await?);
    let membership = bob_phone.cell_membership_for_test(bob, cell).await?;
    assert_eq!(
        membership.member(&bob).map(data_layer::Member::run),
        Some(1)
    );
    // Denied: the burned secret, again.
    let replayed = bob_phone.cells().join(bob, invite).await;
    assert!(replayed.is_err_and(|err| is::<JoinRefused>(&err)));

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A former member invited by a device that has not seen its leave joins
/// again: its joined event goes past the leave, at the run the joiner
/// reports from its own replica.
///
/// Both devices refuse every cell session from the leave on, so the left
/// event stays on the leaving device until the join's catch-up; the
/// inviting device refuses until the join is paused before its catch-up,
/// since a session served earlier would carry the left event to it before
/// it offers the sequence.
#[tokio::test(flavor = "multi_thread")]
async fn a_former_member_invited_by_a_device_that_has_not_seen_its_leave_joins_again() -> Result<()>
{
    let alice_phone = memory_runtime().await?;
    let bob_phone = Arc::new(
        Runtime::spawn(SpawnOptions {
            cell_reconcile_interval: Duration::from_millis(500),
            ..SpawnOptions::memory()
        })
        .await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_members(&alice_phone, alice, cell, both.clone()).await?);

    alice_phone
        .refuse_cell_sessions_for_test(alice, true)
        .await?;
    bob_phone.refuse_cell_sessions_for_test(bob, true).await?;
    bob_phone.cells().act(bob, cell, CellAct::Leave).await?;
    assert!(lists_cell(&bob_phone, bob, cell, false).await?);
    let invite = alice_phone.cells().invite(alice, cell, None).await?;
    let pause = bob_phone.pause_next_join_catch_up().await;
    let joining = {
        let (bob_phone, invite) = (Arc::clone(&bob_phone), invite);
        tokio::spawn(async move { bob_phone.cells().join(bob, invite).await })
    };
    pause.wait_until_reached().await;
    let offered_on = alice_phone.cell_membership_for_test(alice, cell).await?;
    assert_eq!(
        offered_on
            .member(&bob)
            .map(|folded| (folded.run(), folded.state.member)),
        Some((1, true)),
        "the inviting device saw the leave before it made its offer"
    );
    alice_phone
        .refuse_cell_sessions_for_test(alice, false)
        .await?;
    pause.release();

    joining.await??;
    for (runtime, holder) in [(&alice_phone, alice), (&*bob_phone, bob)] {
        assert!(lists_members(runtime, holder, cell, both.clone()).await?);
    }
    let rejoined = bob_phone.cell_membership_for_test(bob, cell).await?;
    assert_eq!(
        rejoined.member(&bob).map(data_layer::Member::run),
        Some(3),
        "the joined event did not go past the leave"
    );
    bob_phone.refuse_cell_sessions_for_test(bob, false).await?;

    alice_phone.shutdown().await?;
    bob_phone.shutdown().await?;
    Ok(())
}

/// A cell with `owner` its creator on `owner_runtime` and `member` joined
/// from `member_runtime`.
async fn cell_of_two(
    owner_runtime: &Runtime,
    owner: PdnId,
    member_runtime: &Runtime,
    member: PdnId,
) -> Result<CellId> {
    let cell = owner_runtime.cells().create(owner).await?;
    let invite = owner_runtime.cells().invite(owner, cell, None).await?;
    member_runtime.cells().join(member, invite).await?;
    Ok(cell)
}

/// Whether `holder` on `runtime` comes to read `want` at `record`.
async fn reads(
    runtime: &Runtime,
    holder: PdnId,
    cell: CellId,
    record: RecordRef,
    want: &[u8],
) -> Result<bool> {
    eventually(|| async {
        Ok(runtime
            .cells()
            .read(holder, cell, record)
            .await
            .ok()
            .flatten()
            .as_deref()
            == Some(want))
    })
    .await
}

/// The writer, operation sequence and payload of each of `record`'s
/// operations `holder` reads, sorted.
async fn operations(
    runtime: &Runtime,
    holder: PdnId,
    cell: CellId,
    record: RecordRef,
) -> Result<Vec<(PdnId, u64, Vec<u8>)>> {
    let mut read: Vec<_> = runtime
        .cells()
        .read_ops(holder, cell, record)
        .await?
        .into_iter()
        .map(|op| (op.id.writer, op.id.op_seq, op.payload))
        .collect();
    read.sort();
    Ok(read)
}

/// Whether `holder` on `runtime` comes to read exactly `want`, sorted, as
/// `record`'s operations.
async fn reads_ops(
    runtime: &Runtime,
    holder: PdnId,
    cell: CellId,
    record: RecordRef,
    mut want: Vec<(PdnId, u64, Vec<u8>)>,
) -> Result<bool> {
    want.sort();
    eventually(|| async {
        Ok(operations(runtime, holder, cell, record)
            .await
            .ok()
            .as_ref()
            == Some(&want))
    })
    .await
}

/// A claim and an immutable-document round-trip unchanged under the name of
/// the member that placed them, and a write addressed at either is refused,
/// by that member and by the owner alike, every member reading the bytes
/// placed first and listing no record beside the two. Denied: a co-located
/// identity that is no member reads nothing of the cell.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_and_an_immutable_document_are_placed_once() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let erin = alice_phone.identity().create().await?;
    let cell = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;

    let mut placed = Vec::new();
    for kind in [RecordKind::Claim, RecordKind::ImmutableDocument] {
        let record = bob_phone
            .cells()
            .put_record(bob, cell, kind, b"placed first")
            .await?;
        assert_eq!((record.member, record.kind), (bob, kind));
        assert!(reads(&alice_phone, alice, cell, record, b"placed first").await?);
        for (runtime, writer) in [(&bob_phone, bob), (&alice_phone, alice)] {
            let refused = runtime
                .cells()
                .append_op(writer, cell, record, b"placed again")
                .await;
            assert!(refused.is_err_and(|err| is::<RecordPlacedOnce>(&err)));
            assert_eq!(
                runtime.cells().read(writer, cell, record).await?.as_deref(),
                Some(&b"placed first"[..])
            );
        }
        let refused = alice_phone.cells().read_ops(alice, cell, record).await;
        assert!(refused.is_err_and(|err| is::<WrongRecordKind>(&err)));
        // Denied (outsider).
        let refused = alice_phone.cells().read(erin, cell, record).await;
        assert!(refused.is_err_and(|err| is::<UnknownCell>(&err)));
        placed.push(record);
    }
    placed.sort();
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        assert_eq!(runtime.cells().list_records(holder, cell).await?, placed);
    }
    let refused = alice_phone.cells().list_records(erin, cell).await;
    assert!(refused.is_err_and(|err| is::<UnknownCell>(&err)));

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A plain member's operations on the owner's mergeable-document, three of
/// them appended at once, read on the owner's device as that member's, each
/// under an operation sequence of its own. Denied: a co-located identity that
/// is no member edits and reads nothing, and an operation addressed at a
/// mergeable-document the cell does not hold is refused, so none is created
/// under another member's name.
#[tokio::test(flavor = "multi_thread")]
async fn any_member_edits_another_members_mergeable_document() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let erin = bob_phone.identity().create().await?;
    let cell = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let note = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::MergeableDocument, b"milk")
        .await?;
    let first = vec![(alice, 1, b"milk".to_vec())];
    assert!(reads_ops(&bob_phone, bob, cell, note, first.clone()).await?);

    let cells = bob_phone.cells();
    let (one, two, three) = tokio::join!(
        cells.append_op(bob, cell, note, b"eggs"),
        cells.append_op(bob, cell, note, b"eggs"),
        cells.append_op(bob, cell, note, b"eggs"),
    );
    for appended in [one, two, three] {
        appended?;
    }
    let mut edited = first;
    edited.extend((1..=3).map(|op_seq| (bob, op_seq, b"eggs".to_vec())));
    assert!(reads_ops(&alice_phone, alice, cell, note, edited.clone()).await?);

    // Denied (outsider).
    let refused = cells.append_op(erin, cell, note, b"cake").await;
    assert!(refused.is_err_and(|err| is::<UnknownCell>(&err)));
    let refused = cells.read_ops(erin, cell, note).await;
    assert!(refused.is_err_and(|err| is::<UnknownCell>(&err)));
    // Denied: a record the cell does not hold.
    let absent = RecordRef {
        id: RecordId::from_bytes([0x77; 16]),
        ..note
    };
    let refused = cells.append_op(bob, cell, absent, b"cake").await;
    assert!(refused.is_err_and(|err| is::<UnknownRecord>(&err)));
    let refused = cells.read_ops(bob, cell, absent).await;
    assert!(refused.is_err_and(|err| is::<UnknownRecord>(&err)));
    // Sentinel: an operation appended after the refusals reaches the owner.
    cells.append_op(bob, cell, note, b"bread").await?;
    edited.push((bob, 4, b"bread".to_vec()));
    assert!(reads_ops(&alice_phone, alice, cell, note, edited).await?);
    assert_eq!(alice_phone.cells().list_records(alice, cell).await?, [note]);

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Three identities with no connection to one another share through one
/// cell: a claim one invited member places reads on the other's device, and
/// none of the three lists a connection. Denied: an identity on that
/// device's node holding a connection with the claim's author and no
/// membership lists no such cell and reads nothing of it.
#[tokio::test(flavor = "multi_thread")]
async fn three_identities_share_through_a_cell_with_no_connections() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let cell = cell_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    let claim = bob_phone
        .cells()
        .put_record(bob, cell, RecordKind::Claim, b"lease scan")
        .await?;
    assert!(reads(&carol_phone, carol, cell, claim, b"lease scan").await?);
    for (runtime, holder) in [
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    ] {
        assert!(runtime.connections().list(holder).await?.is_empty());
    }

    // Denied: a connection with the author, no membership.
    let erin = carol_phone.identity().create().await?;
    let invite = bob_phone.connections().invite(bob, None).await?;
    carol_phone.connections().establish(erin, invite).await?;
    assert_eq!(carol_phone.connections().list(erin).await?, [bob]);
    assert!(carol_phone.cells().list(erin).await?.is_empty());
    let refused = carol_phone.cells().read(erin, cell, claim).await;
    assert!(refused.is_err_and(|err| is::<UnknownCell>(&err)));

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Two cells with the same members keep their entries apart: a claim placed
/// in each reads there on the other member's device, and neither cell's
/// stores hold an entry of the other's claim. Denied: each claim addressed
/// at the other cell, by either member, reads nothing.
#[tokio::test(flavor = "multi_thread")]
async fn two_cells_with_the_same_members_keep_their_entries_apart() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let household = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let taxes = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let lease = bob_phone
        .cells()
        .put_record(bob, taxes, RecordKind::Claim, b"lease scan")
        .await?;
    let shopping = bob_phone
        .cells()
        .put_record(bob, household, RecordKind::Claim, b"shopping list")
        .await?;
    assert!(reads(&alice_phone, alice, taxes, lease, b"lease scan").await?);
    assert!(reads(&alice_phone, alice, household, shopping, b"shopping list").await?);

    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        for (cell, own) in [(taxes, lease), (household, shopping)] {
            assert_eq!(runtime.cells().list_records(holder, cell).await?, [own]);
            let view = runtime.cell_record_view_for_test(holder, cell).await?;
            let held: Vec<_> = view
                .verdicts()
                .map(|(entry, _)| RecordKey::parse(&entry.key).map(|key| key.record()))
                .collect();
            assert_eq!(held, [Some(own)]);
            assert!(runtime.cells().list_unknown(holder, cell).await?.is_empty());
        }
        // Denied: the other cell's claim.
        assert_eq!(runtime.cells().read(holder, household, lease).await?, None);
        assert_eq!(runtime.cells().read(holder, taxes, shopping).await?, None);
    }

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A cell `alice` created on `alice_phone`, `bob` and `carol` invited by her.
async fn cell_of_three(
    (alice_phone, alice): (&Runtime, PdnId),
    (bob_phone, bob): (&Runtime, PdnId),
    (carol_phone, carol): (&Runtime, PdnId),
) -> Result<CellId> {
    let cell = cell_of_two(alice_phone, alice, bob_phone, bob).await?;
    let invite = alice_phone.cells().invite(alice, cell, None).await?;
    carol_phone.cells().join(carol, invite).await?;
    Ok(cell)
}

fn refused(acted: Result<()>, reason: ActRefusal) -> bool {
    acted.is_err_and(|err| {
        err.downcast_ref::<ActRefused>()
            .is_some_and(|refusal| refusal.reason == reason)
    })
}

/// Whether `holder` on `runtime` comes to list `cell` among its cells, or
/// to list it no longer.
async fn lists_cell(runtime: &Runtime, holder: PdnId, cell: CellId, held: bool) -> Result<bool> {
    eventually(|| async {
        let listed = runtime.cells().list(holder).await?;
        Ok(listed.iter().any(|info| info.id == cell) == held)
    })
    .await
}

/// An owner's promotion of a plain member reaches the third member, who
/// lists both owners. Denied: that third member's promotion of itself and
/// demotion of the owner, both refused before the promotion, leave every
/// member listing the roles the promotion alone gives.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_promotes_a_member_and_a_plain_member_promotes_nobody() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let cell = cell_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;

    // Denied (a plain member).
    let cells = carol_phone.cells();
    let promoted = cells.act(carol, cell, CellAct::Promote(carol)).await;
    assert!(refused(promoted, ActRefusal::NotAnOwner));
    let demoted = cells.act(carol, cell, CellAct::Demote(alice)).await;
    assert!(refused(demoted, ActRefusal::NotAnOwner));

    alice_phone
        .cells()
        .act(alice, cell, CellAct::Promote(bob))
        .await?;
    let roles = vec![member(alice, true), member(bob, true), member(carol, false)];
    for (runtime, holder) in [
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    ] {
        assert!(lists_members(runtime, holder, cell, roles.clone()).await?);
    }

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// An owner demotes another owner, who stays a member. Denied: an owner's
/// demotion of itself, the demoted owner's demotion of the other, and a
/// demotion of a plain member, each refused with nothing written.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_demotes_another_owner_and_not_itself() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    alice_phone
        .cells()
        .act(alice, cell, CellAct::Promote(bob))
        .await?;
    let owners = vec![member(alice, true), member(bob, true)];
    assert!(lists_members(&bob_phone, bob, cell, owners.clone()).await?);

    // Denied: on itself.
    let demoted = alice_phone
        .cells()
        .act(alice, cell, CellAct::Demote(alice))
        .await;
    assert!(refused(demoted, ActRefusal::OnItself));
    assert_eq!(alice_phone.cells().members(alice, cell).await?, {
        let mut owners = owners;
        owners.sort();
        owners
    });

    bob_phone
        .cells()
        .act(bob, cell, CellAct::Demote(alice))
        .await?;
    let demoted = vec![member(alice, false), member(bob, true)];
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        assert!(lists_members(runtime, holder, cell, demoted.clone()).await?);
    }
    // Denied: a demoted owner, and a demotion of a plain member.
    let acted = alice_phone
        .cells()
        .act(alice, cell, CellAct::Demote(bob))
        .await;
    assert!(refused(acted, ActRefusal::NotAnOwner));
    let acted = bob_phone
        .cells()
        .act(bob, cell, CellAct::Demote(alice))
        .await;
    assert!(refused(acted, ActRefusal::SubjectNotOwner));
    assert!(lists_members(&bob_phone, bob, cell, demoted).await?);

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// An owner kicked by another owner learns of the kick and stops listing
/// the cell; invited again it joins as a plain member, every member listing
/// it so, until an owner promotes it anew and its demotion of that owner
/// goes through. Denied: the same demotion between the rejoin and the new
/// promotion.
#[tokio::test(flavor = "multi_thread")]
async fn a_former_owner_kicked_and_invited_again_is_a_plain_member() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    alice_phone
        .cells()
        .act(alice, cell, CellAct::Promote(bob))
        .await?;
    let owners = vec![member(alice, true), member(bob, true)];
    assert!(lists_members(&bob_phone, bob, cell, owners).await?);

    alice_phone
        .cells()
        .act(alice, cell, CellAct::Kick(bob))
        .await?;
    assert_eq!(
        alice_phone.cells().members(alice, cell).await?,
        [member(alice, true)]
    );
    assert!(
        lists_cell(&bob_phone, bob, cell, false).await?,
        "the kicked owner's device did not learn of its kick"
    );
    let asked = bob_phone.cells().members(bob, cell).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));

    let invite = alice_phone.cells().invite(alice, cell, None).await?;
    bob_phone.cells().join(bob, invite).await?;
    let rejoined = vec![member(alice, true), member(bob, false)];
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        assert!(lists_members(runtime, holder, cell, rejoined.clone()).await?);
    }
    assert!(lists_cell(&bob_phone, bob, cell, true).await?);
    // Denied: an owner's act before an owner promotes it anew.
    let demoted = bob_phone
        .cells()
        .act(bob, cell, CellAct::Demote(alice))
        .await;
    assert!(refused(demoted, ActRefusal::NotAnOwner));
    assert!(lists_members(&alice_phone, alice, cell, rejoined).await?);

    alice_phone
        .cells()
        .act(alice, cell, CellAct::Promote(bob))
        .await?;
    let owners = vec![member(alice, true), member(bob, true)];
    assert!(lists_members(&bob_phone, bob, cell, owners).await?);
    bob_phone
        .cells()
        .act(bob, cell, CellAct::Demote(alice))
        .await?;
    let demoted = vec![member(alice, false), member(bob, true)];
    assert!(lists_members(&alice_phone, alice, cell, demoted).await?);

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// An owner's kick of a plain member reaches the remaining member, the two
/// still syncing, and the kicked member's device stops listing the cell.
/// Denied: the kicked member's kick of the remaining one, refused while it
/// was a plain member, and the owner's kick of itself, the remaining member
/// still listed and served.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_kicks_a_member_and_a_plain_member_kicks_nobody() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let cell = cell_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;

    // Denied (a plain member, and on itself).
    let kicked = carol_phone
        .cells()
        .act(carol, cell, CellAct::Kick(bob))
        .await;
    assert!(refused(kicked, ActRefusal::NotAnOwner));
    let kicked = alice_phone
        .cells()
        .act(alice, cell, CellAct::Kick(alice))
        .await;
    assert!(refused(kicked, ActRefusal::OnItself));

    alice_phone
        .cells()
        .act(alice, cell, CellAct::Kick(carol))
        .await?;
    let remaining = vec![member(alice, true), member(bob, false)];
    assert!(lists_members(&bob_phone, bob, cell, remaining).await?);
    assert!(
        lists_cell(&carol_phone, carol, cell, false).await?,
        "the kicked member's device did not learn of its kick"
    );
    let claim = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"after the kick")
        .await?;
    assert!(reads(&bob_phone, bob, cell, claim, b"after the kick").await?);
    let asked = carol_phone.cells().read(carol, cell, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// The one owner leaves only once another member is an owner, the promotion
/// written just before the leave making that member the one owner on every
/// remaining device; the one member of a cell leaves as any member does.
/// Denied: the one owner's leave while the cell has other members.
#[tokio::test(flavor = "multi_thread")]
async fn the_one_owner_leaves_once_another_member_is_an_owner() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let cell = cell_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    let alone = alice_phone.cells().create(alice).await?;

    // Denied: the one owner.
    let left = alice_phone.cells().act(alice, cell, CellAct::Leave).await;
    assert!(refused(left, ActRefusal::SoleOwner));
    let mut all = vec![
        member(alice, true),
        member(bob, false),
        member(carol, false),
    ];
    all.sort();
    assert_eq!(alice_phone.cells().members(alice, cell).await?, all);

    let cells = alice_phone.cells();
    cells.act(alice, cell, CellAct::Promote(bob)).await?;
    cells.act(alice, cell, CellAct::Leave).await?;
    cells.act(alice, alone, CellAct::Leave).await?;
    assert!(cells.list(alice).await?.is_empty());
    let asked = cells.members(alice, cell).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));
    let left = vec![member(bob, true), member(carol, false)];
    for (runtime, holder) in [(&bob_phone, bob), (&carol_phone, carol)] {
        assert!(lists_members(runtime, holder, cell, left.clone()).await?);
    }

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A member that leaves right after writing stops listing the cell, and
/// the claim and the operation it wrote reach the remaining members; a
/// co-located member's leave then spares the member beside it. Denied: a
/// departed member reads nothing of the cell. The leave flushes to one of
/// the two members on the tablet, and the other takes what it wrote at the
/// tablet's cell pass, run every half second here.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_leaves_and_what_it_wrote_stays() -> Result<()> {
    let tablet = Runtime::spawn(SpawnOptions {
        cell_reconcile_interval: std::time::Duration::from_millis(500),
        ..SpawnOptions::memory()
    })
    .await?;
    let bob_phone = memory_runtime().await?;
    let leisure = tablet.identity().create().await?;
    let work = tablet.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = cell_of_two(&tablet, leisure, &bob_phone, bob).await?;
    let invite = tablet.cells().invite(leisure, cell, None).await?;
    tablet.cells().join(work, invite).await?;
    let note = tablet
        .cells()
        .put_record(leisure, cell, RecordKind::MergeableDocument, b"milk")
        .await?;
    let first = vec![(leisure, 1, b"milk".to_vec())];
    assert!(reads_ops(&bob_phone, bob, cell, note, first).await?);

    let cells = bob_phone.cells();
    let claim = cells
        .put_record(bob, cell, RecordKind::Claim, b"bob's claim")
        .await?;
    cells.append_op(bob, cell, note, b"eggs").await?;
    cells.act(bob, cell, CellAct::Leave).await?;
    assert!(cells.list(bob).await?.is_empty());
    // Denied: the departed member.
    let asked = cells.read(bob, cell, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));
    let edited = vec![(leisure, 1, b"milk".to_vec()), (bob, 1, b"eggs".to_vec())];
    let remaining = vec![member(leisure, true), member(work, false)];
    for holder in [leisure, work] {
        assert!(lists_members(&tablet, holder, cell, remaining.clone()).await?);
        assert!(reads(&tablet, holder, cell, claim, b"bob's claim").await?);
        assert!(reads_ops(&tablet, holder, cell, note, edited.clone()).await?);
    }

    tablet.cells().act(work, cell, CellAct::Leave).await?;
    assert!(tablet.cells().list(work).await?.is_empty());
    let asked = tablet.cells().read(work, cell, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));
    assert!(lists_members(&tablet, leisure, cell, vec![member(leisure, true)]).await?);
    assert!(lists_cell(&tablet, leisure, cell, true).await?);
    assert!(reads(&tablet, leisure, cell, claim, b"bob's claim").await?);

    for runtime in [tablet, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Devices linked into a member before and after its join reach the cell
/// from the identity's directory, read it, and register themselves, so the
/// owner's device reads what they write and serves one of them with every
/// other device of the member gone. Denied: a co-located identity that is
/// no member lists no such cell and reads nothing of it. A linked device's
/// first session can reach its sibling before its confirmation does, and is
/// refused; its cell pass, every half second here, opens the next one.
#[tokio::test(flavor = "multi_thread")]
async fn devices_linked_before_and_after_the_join_reach_the_cell() -> Result<()> {
    let quick = || SpawnOptions {
        cell_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    };
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let (bob_laptop, bob_tablet) = (
        Runtime::spawn(quick()).await?,
        Runtime::spawn(quick()).await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let dave = bob_laptop.identity().create().await?;
    let cell = alice_phone.cells().create(alice).await?;
    let before = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"before")
        .await?;
    link_patiently(&bob_laptop, &bob_phone, bob).await?;
    let invite = alice_phone.cells().invite(alice, cell, None).await?;
    bob_phone.cells().join(bob, invite).await?;
    link_patiently(&bob_tablet, &bob_phone, bob).await?;

    for (device, said) in [(&bob_laptop, &b"laptop"[..]), (&bob_tablet, &b"tablet"[..])] {
        assert!(lists_cell(device, bob, cell, true).await?);
        assert!(reads(device, bob, cell, before, b"before").await?);
        let placed = device
            .cells()
            .put_record(bob, cell, RecordKind::Claim, said)
            .await?;
        assert!(
            reads(&alice_phone, alice, cell, placed, said).await?,
            "the owner's device did not read what a linked device wrote"
        );
    }
    // Denied (a co-located non-member).
    assert!(bob_laptop.cells().list(dave).await?.is_empty());
    let asked = bob_laptop.cells().read(dave, cell, before).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));

    bob_phone.shutdown().await?;
    bob_tablet.shutdown().await?;
    let after = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"after")
        .await?;
    assert!(
        reads(&bob_laptop, bob, cell, after, b"after").await?,
        "the owner's device did not serve the linked device"
    );

    for runtime in [alice_phone, bob_laptop] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A cell's tickets reach a device linked into the identity, each naming
/// the device it was minted on as the identity that holds the stores there:
/// a created cell's two under its own kinds, and a joined cell's two beside
/// the two the inviting device handed over, under the inviter's kinds.
#[tokio::test(flavor = "multi_thread")]
async fn a_cells_tickets_reach_a_linked_device_each_naming_its_holder() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let names = |ticket: &data_layer::DocTicket, runtime: &Runtime, holder: PdnId| {
        let nodes: Vec<[u8; 32]> = ticket
            .nodes
            .iter()
            .map(|addr| *addr.id.as_bytes())
            .collect();
        ticket.identity == data_layer::identity_of(holder)
            && nodes == [*runtime.node_id().as_bytes()]
    };

    for (runtime, holder, inviter) in [
        (&alice_phone, alice, None),
        (&bob_phone, bob, Some((&alice_phone, alice))),
    ] {
        let (probe, directory) = link_probe(runtime, holder).await?;
        for store in [CellStore::Membership, CellStore::Records] {
            let own = cell_ticket_kind(&cell, store);
            assert!(
                eventually(|| async {
                    Ok(directory
                        .get_ticket(&own)
                        .await?
                        .is_some_and(|ticket| names(&ticket, runtime, holder)))
                })
                .await?,
                "the cell's own ticket did not name its device as its holder"
            );
            let handed = cell_inviter_ticket_kind(&cell, store);
            match inviter {
                None => assert!(directory.get_ticket(&handed).await?.is_none()),
                Some((inviting, inviter)) => assert!(
                    eventually(|| async {
                        Ok(directory
                            .get_ticket(&handed)
                            .await?
                            .is_some_and(|ticket| names(&ticket, inviting, inviter)))
                    })
                    .await?,
                    "the inviter's ticket did not name the inviting device as the inviter"
                ),
            }
        }
        probe.shutdown().await?;
    }

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A leave on one of a member's devices, and an owner's kick of the member,
/// each reach the member's other device, which stops listing the cell and
/// reads nothing of it, while the owner goes on.
#[tokio::test(flavor = "multi_thread")]
async fn a_departure_reaches_the_members_other_devices() -> Result<()> {
    let (alice_phone, bob_phone, bob_laptop) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    link_patiently(&bob_laptop, &bob_phone, bob).await?;
    let family = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let wedding = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let claims = [
        alice_phone
            .cells()
            .put_record(alice, family, RecordKind::Claim, b"family")
            .await?,
        alice_phone
            .cells()
            .put_record(alice, wedding, RecordKind::Claim, b"wedding")
            .await?,
    ];
    for (cell, claim, said) in [
        (family, claims[0], &b"family"[..]),
        (wedding, claims[1], &b"wedding"[..]),
    ] {
        assert!(reads(&bob_laptop, bob, cell, claim, said).await?);
    }

    bob_phone.cells().act(bob, family, CellAct::Leave).await?;
    alice_phone
        .cells()
        .act(alice, wedding, CellAct::Kick(bob))
        .await?;
    for (cell, claim) in [(family, claims[0]), (wedding, claims[1])] {
        for device in [&bob_phone, &bob_laptop] {
            assert!(
                lists_cell(device, bob, cell, false).await?,
                "a device of the departed member still lists the cell"
            );
            let asked = device.cells().read(bob, cell, claim).await;
            assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));
        }
        assert!(lists_members(&alice_phone, alice, cell, vec![member(alice, true)]).await?);
    }

    for runtime in [alice_phone, bob_phone, bob_laptop] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A runtime on `dir`, which a scenario stops and starts again.
async fn runtime_on(dir: &std::path::Path) -> Result<Runtime> {
    Runtime::spawn(SpawnOptions::on_directory(dir)).await
}

/// A member's runtime on a storage directory hosts its cell again after a
/// restart, from its directory alone: the claim another member placed
/// meanwhile arrives, the member's next operation continues its author's
/// count, and its hosting record is the one its create wrote.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_is_hosted_again_after_a_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let alice_phone = memory_runtime().await?;
    let bob_tablet = runtime_on(dir.path()).await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_tablet.identity().create().await?;
    let cell = cell_of_two(&alice_phone, alice, &bob_tablet, bob).await?;
    let note = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::MergeableDocument, b"milk")
        .await?;
    let mut edited = vec![(alice, 1, b"milk".to_vec())];
    assert!(reads_ops(&bob_tablet, bob, cell, note, edited.clone()).await?);
    for _ in 0..3 {
        bob_tablet
            .cells()
            .append_op(bob, cell, note, b"eggs")
            .await?;
    }
    edited.extend((1..=3).map(|op_seq| (bob, op_seq, b"eggs".to_vec())));
    assert!(reads_ops(&alice_phone, alice, cell, note, edited.clone()).await?);
    let record = dir
        .path()
        .join("identities")
        .join(bob.to_string())
        .join("directory");
    let recorded = std::fs::read(&record)?;
    bob_tablet.shutdown().await?;
    drop(bob_tablet);

    let meanwhile = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"meanwhile")
        .await?;
    let bob_tablet = runtime_on(dir.path()).await?;
    assert!(lists_cell(&bob_tablet, bob, cell, true).await?);
    assert!(reads(&bob_tablet, bob, cell, meanwhile, b"meanwhile").await?);
    bob_tablet
        .cells()
        .append_op(bob, cell, note, b"bread")
        .await?;
    edited.push((bob, 4, b"bread".to_vec()));
    assert!(reads_ops(&alice_phone, alice, cell, note, edited).await?);
    assert_eq!(
        std::fs::read(&record)?,
        recorded,
        "the restart rewrote the hosting record"
    );

    for runtime in [alice_phone, bob_tablet] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Two members hosted on one node come back from a restart each with its
/// own copy of their cell, and a record one places reaches the other with no
/// other node reachable; a cell the second left before the restart stays
/// left, its membership store alone kept as the tombstone. Denied: the left
/// cell's records, to the member that left it.
#[tokio::test(flavor = "multi_thread")]
async fn co_located_members_come_back_and_a_left_cell_stays_left() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let tablet = runtime_on(dir.path()).await?;
    let leisure = tablet.identity().create().await?;
    let work = tablet.identity().create().await?;
    let wedding = tablet.cells().create(leisure).await?;
    let invite = tablet.cells().invite(leisure, wedding, None).await?;
    tablet.cells().join(work, invite).await?;
    let left = tablet.cells().create(work).await?;
    let before = tablet
        .cells()
        .put_record(work, left, RecordKind::Claim, b"before the leave")
        .await?;
    tablet.cells().act(work, left, CellAct::Leave).await?;
    tablet.shutdown().await?;
    drop(tablet);

    let tablet = runtime_on(dir.path()).await?;
    let both = vec![member(leisure, true), member(work, false)];
    for holder in [leisure, work] {
        assert!(lists_cell(&tablet, holder, wedding, true).await?);
        assert!(lists_members(&tablet, holder, wedding, both.clone()).await?);
    }
    let mut kept = vec![(wedding, true), (left, false)];
    kept.sort();
    assert!(
        eventually(|| async {
            let mut holdings = tablet.cell_holdings_for_test(work).await?;
            holdings.sort();
            Ok(holdings == kept)
        })
        .await?,
        "the left cell came back as more or less than its tombstone"
    );
    assert!(!tablet
        .cells()
        .list(work)
        .await?
        .iter()
        .any(|info| info.id == left));
    // Denied: the left cell.
    let asked = tablet.cells().read(work, left, before).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));

    let claim = tablet
        .cells()
        .put_record(leisure, wedding, RecordKind::Claim, b"after the restart")
        .await?;
    assert!(reads(&tablet, work, wedding, claim, b"after the restart").await?);

    tablet.shutdown().await?;
    Ok(())
}

/// A member that leaves as an owner and is invited again joins as a plain
/// member: it reads its own record from before the leave and the one placed
/// while it was out, a record it places afterwards reaches every member,
/// and once an owner promotes it anew its owner's acts go through. Denied:
/// its owner's act between the rejoin and the new promotion.
#[allow(clippy::too_many_lines)] // one scenario: the leave, the return and the role after it
#[tokio::test(flavor = "multi_thread")]
async fn a_member_that_left_as_an_owner_joins_again_as_a_plain_member() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let cell = cell_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    alice_phone
        .cells()
        .act(alice, cell, CellAct::Promote(bob))
        .await?;
    let owners = vec![member(alice, true), member(bob, true), member(carol, false)];
    assert!(lists_members(&bob_phone, bob, cell, owners).await?);
    let earlier = bob_phone
        .cells()
        .put_record(bob, cell, RecordKind::Claim, b"before the leave")
        .await?;
    assert!(reads(&carol_phone, carol, cell, earlier, b"before the leave").await?);

    bob_phone.cells().act(bob, cell, CellAct::Leave).await?;
    assert!(lists_cell(&bob_phone, bob, cell, false).await?);
    let without = vec![member(alice, true), member(carol, false)];
    assert!(lists_members(&carol_phone, carol, cell, without).await?);
    let away = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"while away")
        .await?;

    let invite = alice_phone.cells().invite(alice, cell, None).await?;
    bob_phone.cells().join(bob, invite).await?;
    let rejoined = vec![
        member(alice, true),
        member(bob, false),
        member(carol, false),
    ];
    for (runtime, holder) in [
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    ] {
        assert!(lists_members(runtime, holder, cell, rejoined.clone()).await?);
    }
    assert!(reads(&bob_phone, bob, cell, earlier, b"before the leave").await?);
    assert!(reads(&bob_phone, bob, cell, away, b"while away").await?);
    let later = bob_phone
        .cells()
        .put_record(bob, cell, RecordKind::Claim, b"after the return")
        .await?;
    for (runtime, holder) in [(&alice_phone, alice), (&carol_phone, carol)] {
        assert!(reads(runtime, holder, cell, later, b"after the return").await?);
    }
    // Denied: an owner's act before an owner promotes it anew.
    let promoted = bob_phone
        .cells()
        .act(bob, cell, CellAct::Promote(carol))
        .await;
    assert!(refused(promoted, ActRefusal::NotAnOwner));

    alice_phone
        .cells()
        .act(alice, cell, CellAct::Promote(bob))
        .await?;
    let promoted_anew = vec![member(alice, true), member(bob, true), member(carol, false)];
    assert!(lists_members(&bob_phone, bob, cell, promoted_anew).await?);
    bob_phone
        .cells()
        .act(bob, cell, CellAct::Promote(carol))
        .await?;
    let all_owners = vec![member(alice, true), member(bob, true), member(carol, true)];
    assert!(lists_members(&carol_phone, carol, cell, all_owners).await?);

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A member promoted, demoted and promoted again kicks as its role at each
/// point allows, every member listing the roles its chain gives: its first
/// kick goes through, and its second once it is an owner again. Denied:
/// the second kick while it is a plain member.
#[allow(clippy::too_many_lines)] // one scenario: three role flips and a kick beside each
#[tokio::test(flavor = "multi_thread")]
async fn a_member_promoted_demoted_and_promoted_again_kicks_as_its_role_allows() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone, dave_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let dave = dave_phone.identity().create().await?;
    let cell = cell_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    let invite = alice_phone.cells().invite(alice, cell, None).await?;
    dave_phone.cells().join(dave, invite).await?;
    let owner_bob =
        |owner: bool| vec![member(alice, true), member(bob, owner), member(dave, false)];

    alice_phone
        .cells()
        .act(alice, cell, CellAct::Promote(bob))
        .await?;
    assert!(
        lists_members(&bob_phone, bob, cell, {
            let mut four = owner_bob(true);
            four.push(member(carol, false));
            four
        })
        .await?
    );
    bob_phone
        .cells()
        .act(bob, cell, CellAct::Kick(carol))
        .await?;
    assert!(lists_members(&alice_phone, alice, cell, owner_bob(true)).await?);

    alice_phone
        .cells()
        .act(alice, cell, CellAct::Demote(bob))
        .await?;
    assert!(lists_members(&bob_phone, bob, cell, owner_bob(false)).await?);
    // Denied (a plain member again).
    let kicked = bob_phone.cells().act(bob, cell, CellAct::Kick(dave)).await;
    assert!(refused(kicked, ActRefusal::NotAnOwner));
    assert!(lists_members(&dave_phone, dave, cell, owner_bob(false)).await?);

    alice_phone
        .cells()
        .act(alice, cell, CellAct::Promote(bob))
        .await?;
    assert!(lists_members(&bob_phone, bob, cell, owner_bob(true)).await?);
    bob_phone
        .cells()
        .act(bob, cell, CellAct::Kick(dave))
        .await?;
    let remaining = vec![member(alice, true), member(bob, true)];
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        assert!(lists_members(runtime, holder, cell, remaining.clone()).await?);
    }
    let membership = alice_phone.cell_membership_for_test(alice, cell).await?;
    assert_eq!(
        membership.member(&bob).map(data_layer::Member::run),
        Some(4)
    );

    for runtime in [alice_phone, bob_phone, carol_phone, dave_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A device linked into a member after an owner's demotion lists what that
/// owner did while an owner: the member it promoted then is an owner, the
/// demoted owner a plain member. Denied: the demoted owner's promotion of
/// itself, refused on its device. The laptop's first session can reach its
/// sibling before the laptop's confirmation does, and is refused; its cell
/// pass, every half second here, opens the next one.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_linked_after_an_owners_demotion_lists_what_it_did_as_an_owner() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice_laptop = Runtime::spawn(SpawnOptions {
        cell_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    })
    .await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let cell = cell_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    alice_phone
        .cells()
        .act(alice, cell, CellAct::Promote(bob))
        .await?;
    let bob_owns = vec![member(alice, true), member(bob, true), member(carol, false)];
    assert!(lists_members(&bob_phone, bob, cell, bob_owns).await?);
    bob_phone
        .cells()
        .act(bob, cell, CellAct::Promote(carol))
        .await?;
    let all_own = vec![member(alice, true), member(bob, true), member(carol, true)];
    assert!(lists_members(&alice_phone, alice, cell, all_own).await?);
    alice_phone
        .cells()
        .act(alice, cell, CellAct::Demote(bob))
        .await?;
    let after = vec![member(alice, true), member(bob, false), member(carol, true)];
    assert!(lists_members(&bob_phone, bob, cell, after.clone()).await?);
    // Denied: the demoted owner.
    let promoted = bob_phone
        .cells()
        .act(bob, cell, CellAct::Promote(bob))
        .await;
    assert!(refused(promoted, ActRefusal::NotAnOwner));

    link_patiently(&alice_laptop, &alice_phone, alice).await?;
    assert!(
        lists_members(&alice_laptop, alice, cell, after).await?,
        "the linked device did not list what the demoted owner did as an owner"
    );

    for runtime in [alice_phone, alice_laptop, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Two members appending to one mergeable-document at once both persist:
/// every member reads both operations, each under its own writer. Denied: a
/// co-located identity that is no member appends nothing.
#[tokio::test(flavor = "multi_thread")]
async fn two_members_editing_one_mergeable_document_at_once_both_persist() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let erin = carol_phone.identity().create().await?;
    let cell = cell_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    let note = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::MergeableDocument, b"milk")
        .await?;
    let first = vec![(alice, 1, b"milk".to_vec())];
    for (runtime, holder) in [(&bob_phone, bob), (&carol_phone, carol)] {
        assert!(reads_ops(runtime, holder, cell, note, first.clone()).await?);
    }

    let (bobs, carols) = (bob_phone.cells(), carol_phone.cells());
    let (eggs, bread) = tokio::join!(
        bobs.append_op(bob, cell, note, b"eggs"),
        carols.append_op(carol, cell, note, b"bread"),
    );
    eggs?;
    bread?;
    let edited = vec![
        (alice, 1, b"milk".to_vec()),
        (bob, 1, b"eggs".to_vec()),
        (carol, 1, b"bread".to_vec()),
    ];
    for (runtime, holder) in [
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    ] {
        assert!(reads_ops(runtime, holder, cell, note, edited.clone()).await?);
    }
    // Denied (a co-located non-member).
    let refused = carol_phone
        .cells()
        .append_op(erin, cell, note, b"cake")
        .await;
    assert!(refused.is_err_and(|err| is::<UnknownCell>(&err)));
    assert!(reads_ops(&alice_phone, alice, cell, note, edited).await?);

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Whether `holder`'s replica of `cell` on `runtime` comes to hold an entry
/// of `record` that reads nothing.
async fn holds_unread(
    runtime: &Runtime,
    holder: PdnId,
    cell: CellId,
    record: RecordRef,
) -> Result<bool> {
    eventually(|| async {
        let view = runtime.cell_record_view_for_test(holder, cell).await?;
        let held = view.verdicts().any(|(entry, verdict)| {
            verdict != Verdict::Counted
                && RecordKey::parse(&entry.key).is_some_and(|key| key.record() == record)
        });
        Ok(held)
    })
    .await
}

/// A linked device whose device statement did not land in one of its
/// member's two cells before a restart writes it after the restart, and
/// another member reads what the device placed there. Paired denial:
/// before the restart that member holds the device's claim in that cell
/// and reads nothing of it, while it reads the device's claim in the cell
/// the statement reached. A statement write failing in the one cell stands
/// in for a process ended between the two writes.
#[allow(clippy::too_many_lines)] // one scenario: the cut fan-out, the restart and the cell it heals
#[tokio::test(flavor = "multi_thread")]
async fn a_fan_out_cut_by_a_restart_is_healed_by_the_sweep() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let quick = || SpawnOptions {
        cell_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    };
    let (alice_phone, bob_phone) = (
        Runtime::spawn(quick()).await?,
        Runtime::spawn(quick()).await?,
    );
    let on_dir = || SpawnOptions {
        cell_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::on_directory(dir.path())
    };
    let bob_laptop = Runtime::spawn(on_dir()).await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let family = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let wedding = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;

    bob_laptop.fail_device_statements_for_test(wedding).await;
    link_patiently(&bob_laptop, &bob_phone, bob).await?;
    let mut placed = Vec::new();
    let both = vec![member(alice, true), member(bob, false)];
    for (cell, said) in [(family, &b"family"[..]), (wedding, &b"wedding"[..])] {
        assert!(lists_members(&bob_laptop, bob, cell, both.clone()).await?);
        let record = bob_laptop
            .cells()
            .put_record(bob, cell, RecordKind::Claim, said)
            .await?;
        placed.push((cell, record, said));
    }
    let [(_, in_family, _), (_, in_wedding, _)] = placed.as_slice() else {
        anyhow::bail!("two claims placed, {} listed", placed.len());
    };
    assert!(reads(&alice_phone, alice, family, *in_family, b"family").await?);
    // Denied: the device no statement in the cell lists.
    assert!(holds_unread(&alice_phone, alice, wedding, *in_wedding).await?);
    let asked = alice_phone
        .cells()
        .read(alice, wedding, *in_wedding)
        .await?;
    assert!(asked.is_none());

    bob_laptop.shutdown().await?;
    drop(bob_laptop);
    let bob_laptop = Runtime::spawn(on_dir()).await?;
    for (cell, record, said) in &placed {
        assert!(
            reads(&alice_phone, alice, *cell, *record, said).await?,
            "the sweep after the restart did not list the device"
        );
    }

    for runtime in [alice_phone, bob_phone, bob_laptop] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A device linked into a member after its leave takes the cell's tombstone
/// from its sibling: it holds the membership store alone, folding its
/// member there as no member, and lists no such cell. Denied: it reads
/// nothing of the cell. A tombstone, out of the swarm, is reconciled by the
/// cell pass alone, which runs every half second on the laptop here.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_linked_after_the_departure_takes_the_tombstone_from_a_sibling() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let bob_laptop = Runtime::spawn(SpawnOptions {
        cell_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    })
    .await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let cell = cell_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let claim = alice_phone
        .cells()
        .put_record(alice, cell, RecordKind::Claim, b"before the leave")
        .await?;
    assert!(reads(&bob_phone, bob, cell, claim, b"before the leave").await?);
    bob_phone.cells().act(bob, cell, CellAct::Leave).await?;

    link_patiently(&bob_laptop, &bob_phone, bob).await?;
    let left = MemberState {
        member: false,
        owner: false,
    };
    assert!(
        eventually(|| async {
            let holdings = bob_laptop.cell_holdings_for_test(bob).await?;
            let folded = bob_laptop.cell_membership_for_test(bob, cell).await;
            Ok(holdings == [(cell, false)]
                && folded.is_ok_and(|membership| {
                    membership.member(&bob).map(|bob| bob.state) == Some(left)
                }))
        })
        .await?,
        "the linked device did not take the tombstone from its sibling"
    );
    assert!(bob_laptop.cells().list(bob).await?.is_empty());
    // Denied: the departed member's new device.
    let asked = bob_laptop.cells().read(bob, cell, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownCell>(&err)));

    for runtime in [alice_phone, bob_phone, bob_laptop] {
        runtime.shutdown().await?;
    }
    Ok(())
}
