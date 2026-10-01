//! The cells service end to end: creating a cell, listing cells and their
//! members, the invite and join dialogue between in-process runtimes — the
//! invite passed as a value — with the refusals of its verify-and-burn each
//! probed for no observable state beside its allowed counterpart, the
//! membership acts each beside the same act refused, and the records
//! members place, edit and read.

use anyhow::Result;
use pdn_node::{
    ActRefusal, ActRefused, CellAct, CellInvite, CellMember, CellsService as _,
    IdentityService as _, JoinRefused, RecordPlacedOnce, Runtime, UnknownCell, UnknownIdentity,
    UnknownRecord, UnsupportedCellInviteVersion, WrongRecordKind,
};
use pdn_types::{CellId, PdnId, RecordId, RecordKind, RecordRef};
use test_utils::{eventually, ids};

mod common;
use common::memory_runtime;

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
/// the cell; invited again it joins as a plain member, and every member
/// lists it so. Denied: its promotion of the owner after the rejoin.
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
    let promoted = bob_phone
        .cells()
        .act(bob, cell, CellAct::Promote(alice))
        .await;
    assert!(refused(promoted, ActRefusal::NotAnOwner));

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
/// departed member reads nothing of the cell.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_leaves_and_what_it_wrote_stays() -> Result<()> {
    let (tablet, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
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
