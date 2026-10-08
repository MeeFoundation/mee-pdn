//! The pods service end to end: creating a pod, listing pods and their
//! members, the invite and join dialogue between in-process runtimes — the
//! invite passed as a value — with the refusals of its verify-and-burn each
//! probed for no observable state beside its allowed counterpart, the
//! membership acts each beside the same act refused, and the records
//! members place, edit and read.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use data_layer::{
    pod_inviter_ticket_kind, pod_ticket_kind, MemberDevice, MemberState, PodStore, RecordKey,
    Verdict,
};
use pdn_node::{
    ActRefusal, ActRefused, ConnectionsService as _, IdentityService as _, JoinRefused, PodAct,
    PodInvite, PodMember, PodsService as _, RecordPlacedOnce, Runtime, SpawnOptions,
    UnknownIdentity, UnknownPod, UnknownRecord, UnsupportedPodInviteVersion, WrongRecordKind,
};
use pdn_types::{PdnId, PodId, RecordId, RecordKind, RecordRef};
use test_utils::{eventually, ids};

mod common;
use common::{link_patiently, link_probe, memory_runtime};

fn member(id: PdnId, owner: bool) -> PodMember {
    PodMember { id, owner }
}

/// Whether `holder` on `runtime` comes to list exactly `want` as the
/// members of `pod`.
async fn lists_members(
    runtime: &Runtime,
    holder: PdnId,
    pod: PodId,
    mut want: Vec<PodMember>,
) -> Result<bool> {
    want.sort();
    eventually(|| async {
        Ok(runtime.pods().members(holder, pod).await.ok() == Some(want.clone()))
    })
    .await
}

fn is<E: std::error::Error + Send + Sync + 'static>(err: &anyhow::Error) -> bool {
    err.downcast_ref::<E>().is_some()
}

/// A created pod is listed with its creator as its one member and owner,
/// and a second pod of the same identity is a second id. Denied: a create
/// for an identity the runtime does not host, and the members of a pod
/// asked by a co-located identity that is no member of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_created_pod_is_listed_with_its_creator_as_owner() -> Result<()> {
    let phone = memory_runtime().await?;
    let alice = phone.identity().create().await?;
    let family = phone.pods().create(alice).await?;
    let wedding = phone.pods().create(alice).await?;
    assert_ne!(family, wedding);
    let mut pods: Vec<PodId> = phone
        .pods()
        .list(alice)
        .await?
        .into_iter()
        .map(|info| info.id)
        .collect();
    let mut expected = vec![family, wedding];
    pods.sort();
    expected.sort();
    assert_eq!(pods, expected);
    for pod in [family, wedding] {
        assert_eq!(
            phone.pods().members(alice, pod).await?,
            vec![member(alice, true)]
        );
    }

    // Denied: an identity the runtime does not host.
    let refused = phone.pods().create(ids::DAVE).await;
    assert!(refused.is_err_and(|err| is::<UnknownIdentity>(&err)));
    // Denied: a co-located identity that is no member.
    let erin = phone.identity().create().await?;
    let asked = phone.pods().members(erin, family).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));
    assert!(phone.pods().list(erin).await?.is_empty());

    phone.shutdown().await?;
    Ok(())
}

/// A newcomer joins with an invite, catches up, and both devices list it
/// among the members, a plain one. Denied: the same secret presented again,
/// by a third identity, is refused, and the pod's members are exactly what
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
    let pod = alice_phone.pods().create(alice).await?;
    let invite = alice_phone.pods().invite(alice, pod, None).await?;

    assert_eq!(bob_phone.pods().join(bob, invite.clone()).await?, pod);
    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_members(&bob_phone, bob, pod, both.clone()).await?);
    assert!(lists_members(&alice_phone, alice, pod, both.clone()).await?);
    assert!(bob_phone
        .pods()
        .list(bob)
        .await?
        .iter()
        .any(|info| info.id == pod));

    // Denied: the burned secret, again.
    let replayed = carol_phone.pods().join(carol, invite).await;
    assert!(replayed.is_err_and(|err| is::<JoinRefused>(&err)));
    assert_eq!(alice_phone.pods().members(alice, pod).await?, {
        let mut both = both;
        both.sort();
        both
    });
    assert!(carol_phone.pods().list(carol).await?.is_empty());

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
    let pod = alice_phone.pods().create(alice).await?;
    let from_alice = alice_phone.pods().invite(alice, pod, None).await?;
    bob_phone.pods().join(bob, from_alice).await?;
    let from_bob = bob_phone.pods().invite(bob, pod, None).await?;
    carol_phone.pods().join(carol, from_bob).await?;

    let all = vec![
        member(alice, true),
        member(bob, false),
        member(carol, false),
    ];
    assert!(
        lists_members(&alice_phone, alice, pod, all).await?,
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
    let pod = alice_phone.pods().create(alice).await?;
    let invite = alice_phone.pods().invite(alice, pod, None).await?;

    let wrong = PodInvite {
        secret: [0x5a; 32],
        ..invite.clone()
    };
    let refused = bob_phone.pods().join(bob, wrong).await;
    assert!(refused.is_err_and(|err| is::<JoinRefused>(&err)));
    assert_eq!(
        alice_phone.pods().members(alice, pod).await?,
        vec![member(alice, true)]
    );
    assert!(bob_phone.pods().list(bob).await?.is_empty());
    let expired = alice_phone
        .pods()
        .invite(alice, pod, Some(std::time::Duration::from_millis(1)))
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let refused = bob_phone.pods().join(bob, expired).await;
    assert!(refused.is_err_and(|err| is::<JoinRefused>(&err)));
    assert!(bob_phone.pods().list(bob).await?.is_empty());
    let unknown_version = PodInvite {
        version: 9,
        ..invite.clone()
    };
    let refused = bob_phone.pods().join(bob, unknown_version).await;
    assert!(refused.is_err_and(|err| is::<UnsupportedPodInviteVersion>(&err)));

    bob_phone.pods().join(bob, invite).await?;
    assert!(
        lists_members(
            &alice_phone,
            alice,
            pod,
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
    let pod = tablet.pods().create(leisure).await?;
    let invite = tablet.pods().invite(leisure, pod, None).await?;
    tablet.pods().join(work, invite.clone()).await?;

    let both = vec![member(leisure, true), member(work, false)];
    for holder in [leisure, work] {
        assert!(lists_members(&tablet, holder, pod, both.clone()).await?);
    }
    // Denied: the burned secret, by a third co-located identity.
    let replayed = tablet.pods().join(erin, invite).await;
    assert!(replayed.is_err_and(|err| is::<JoinRefused>(&err)));
    assert!(tablet.pods().list(erin).await?.is_empty());

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
    let pod = tablet.pods().create(leisure).await?;
    let invite = tablet.pods().invite(leisure, pod, None).await?;
    tablet.pods().join(work, invite).await?;

    let pods = tablet.pods();
    let claim = pods
        .put_record(leisure, pod, RecordKind::Claim, b"leisure's claim")
        .await?;
    let scan = pods
        .put_record(work, pod, RecordKind::ImmutableDocument, b"work's scan")
        .await?;
    let note = pods
        .put_record(leisure, pod, RecordKind::MergeableDocument, b"milk")
        .await?;
    let first = vec![(leisure, 1, b"milk".to_vec())];
    assert!(reads_ops(&tablet, work, pod, note, first).await?);
    pods.append_op(work, pod, note, b"eggs").await?;
    let edited = vec![(leisure, 1, b"milk".to_vec()), (work, 1, b"eggs".to_vec())];
    for holder in [leisure, work] {
        assert!(reads(&tablet, holder, pod, claim, b"leisure's claim").await?);
        assert!(reads(&tablet, holder, pod, scan, b"work's scan").await?);
        assert!(reads_ops(&tablet, holder, pod, note, edited.clone()).await?);
    }
    let membership = tablet.pod_membership_view_for_test(work, pod).await?;
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
    for op in pods.read_ops(leisure, pod, note).await? {
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
    let asked = pods.read(erin, pod, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));

    pods.act(work, pod, PodAct::Leave).await?;
    assert!(pods.list(work).await?.is_empty());
    assert_eq!(tablet.pod_holdings_for_test(work).await?, [(pod, false)]);
    // Denied: the member that left.
    let asked = pods.read(work, pod, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));
    assert!(lists_members(&tablet, leisure, pod, vec![member(leisure, true)]).await?);
    assert!(reads(&tablet, leisure, pod, scan, b"work's scan").await?);
    let later = pods
        .put_record(leisure, pod, RecordKind::Claim, b"after the leave")
        .await?;
    assert!(reads(&tablet, leisure, pod, later, b"after the leave").await?);

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
    let pod = alice_phone.pods().create(alice).await?;

    alice_phone.drop_next_join_reply_for_test().await;
    let first = alice_phone.pods().invite(alice, pod, None).await?;
    let lost = bob_phone.pods().join(bob, first).await;
    assert!(lost.is_err_and(|err| is::<JoinRefused>(&err)));
    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_members(&alice_phone, alice, pod, both.clone()).await?);
    assert!(bob_phone.pods().list(bob).await?.is_empty());

    let second = alice_phone.pods().invite(alice, pod, None).await?;
    bob_phone.pods().join(bob, second).await?;
    assert!(lists_members(&bob_phone, bob, pod, both).await?);
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        let membership = runtime.pod_membership_view_for_test(holder, pod).await?;
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
/// tickets and the PMS's entry are recorded, ends caught up all the
/// same: the newcomer lists the pod and its members and reads what was
/// placed before and after, by the one joined event. Denied: the invite
/// presented again.
#[tokio::test(flavor = "multi_thread")]
async fn a_join_whose_future_is_dropped_during_its_catch_up_is_finished() -> Result<()> {
    let alice_phone = memory_runtime().await?;
    let bob_phone = Arc::new(memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = alice_phone.pods().create(alice).await?;
    let before = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"before")
        .await?;
    let invite = alice_phone.pods().invite(alice, pod, None).await?;

    let pause = bob_phone.pause_next_join_catch_up().await;
    let joining = {
        let (bob_phone, invite) = (Arc::clone(&bob_phone), invite.clone());
        tokio::spawn(async move { bob_phone.pods().join(bob, invite).await })
    };
    pause.wait_until_reached().await;
    joining.abort();
    assert!(joining.await.unwrap_err().is_cancelled());
    pause.release();

    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_pod(&bob_phone, bob, pod, true).await?);
    assert!(lists_members(&bob_phone, bob, pod, both).await?);
    assert!(reads(&bob_phone, bob, pod, before, b"before").await?);
    let after = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"after")
        .await?;
    assert!(reads(&bob_phone, bob, pod, after, b"after").await?);
    let membership = bob_phone.pod_membership_view_for_test(bob, pod).await?;
    assert_eq!(
        membership.member(&bob).map(data_layer::Member::run),
        Some(1)
    );
    // Denied: the burned secret, again.
    let replayed = bob_phone.pods().join(bob, invite).await;
    assert!(replayed.is_err_and(|err| is::<JoinRefused>(&err)));

    alice_phone.shutdown().await?;
    bob_phone.shutdown().await?;
    Ok(())
}

/// A join cut by a restart during its catch-up, once both tickets and the
/// PMS's entry are recorded, is finished by the armer after the
/// restart: the newcomer lists the pod and its members and reads what was
/// placed before the join and while it was down, by the one joined event.
/// Denied: the invite presented again.
#[tokio::test(flavor = "multi_thread")]
async fn a_join_cut_by_a_restart_during_its_catch_up_is_finished_by_the_armer() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let alice_phone = memory_runtime().await?;
    let bob_tablet = Arc::new(runtime_on(dir.path()).await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_tablet.identity().create().await?;
    let pod = alice_phone.pods().create(alice).await?;
    let before = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"before")
        .await?;
    let invite = alice_phone.pods().invite(alice, pod, None).await?;

    let pause = bob_tablet.pause_next_join_catch_up().await;
    let joining = {
        let (bob_tablet, invite) = (Arc::clone(&bob_tablet), invite.clone());
        tokio::spawn(async move { bob_tablet.pods().join(bob, invite).await })
    };
    pause.wait_until_reached().await;
    // The process ends: the `join` future with it, then the node.
    joining.abort();
    assert!(joining.await.unwrap_err().is_cancelled());
    bob_tablet.shutdown().await?;
    drop(bob_tablet);

    let meanwhile = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"meanwhile")
        .await?;
    let bob_tablet = runtime_on(dir.path()).await?;
    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_pod(&bob_tablet, bob, pod, true).await?);
    assert!(lists_members(&bob_tablet, bob, pod, both).await?);
    for (record, said) in [(before, &b"before"[..]), (meanwhile, &b"meanwhile"[..])] {
        assert!(
            reads(&bob_tablet, bob, pod, record, said).await?,
            "the armer did not catch the pod up after the restart"
        );
    }
    let membership = bob_tablet.pod_membership_view_for_test(bob, pod).await?;
    assert_eq!(
        membership.member(&bob).map(data_layer::Member::run),
        Some(1)
    );
    // Denied: the burned secret, again.
    let replayed = bob_tablet.pods().join(bob, invite).await;
    assert!(replayed.is_err_and(|err| is::<JoinRefused>(&err)));

    for runtime in [alice_phone, bob_tablet] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A join whose inviter goes out of reach for its whole catch-up fails with
/// the catch-up timeout and leaves both tickets and the PMS's entry
/// recorded; once the inviter is reachable again the newcomer's pod pass
/// catches it up, by the one joined event. Denied: the invite presented
/// again. The inviter refusing every pod session stands in for the
/// connection gone, since the dialogue runs on its own protocol; the
/// catch-up's whole budget passes.
#[tokio::test(flavor = "multi_thread")]
async fn a_join_whose_inviter_drops_out_during_its_catch_up_is_finished_later() -> Result<()> {
    let alice_phone = memory_runtime().await?;
    let bob_phone = Runtime::spawn(SpawnOptions {
        pod_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    })
    .await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = alice_phone.pods().create(alice).await?;
    let before = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"before")
        .await?;
    let invite = alice_phone.pods().invite(alice, pod, None).await?;

    alice_phone
        .refuse_pod_sessions_for_test(alice, true)
        .await?;
    let cut = bob_phone.pods().join(bob, invite.clone()).await;
    assert!(cut.is_err_and(|err| is::<data_layer::CatchUpTimeout>(&err)));
    assert!(lists_pod(&bob_phone, bob, pod, true).await?);
    alice_phone
        .refuse_pod_sessions_for_test(alice, false)
        .await?;

    let both = vec![member(alice, true), member(bob, false)];
    assert!(
        lists_members(&bob_phone, bob, pod, both).await?,
        "the pod pass did not catch the cut join up"
    );
    assert!(reads(&bob_phone, bob, pod, before, b"before").await?);
    let membership = bob_phone.pod_membership_view_for_test(bob, pod).await?;
    assert_eq!(
        membership.member(&bob).map(data_layer::Member::run),
        Some(1)
    );
    // Denied: the burned secret, again.
    let replayed = bob_phone.pods().join(bob, invite).await;
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
/// Both devices refuse every pod session from the leave on, so the left
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
            pod_reconcile_interval: Duration::from_millis(500),
            ..SpawnOptions::memory()
        })
        .await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let both = vec![member(alice, true), member(bob, false)];
    assert!(lists_members(&alice_phone, alice, pod, both.clone()).await?);

    alice_phone
        .refuse_pod_sessions_for_test(alice, true)
        .await?;
    bob_phone.refuse_pod_sessions_for_test(bob, true).await?;
    bob_phone.pods().act(bob, pod, PodAct::Leave).await?;
    assert!(lists_pod(&bob_phone, bob, pod, false).await?);
    let invite = alice_phone.pods().invite(alice, pod, None).await?;
    let pause = bob_phone.pause_next_join_catch_up().await;
    let joining = {
        let (bob_phone, invite) = (Arc::clone(&bob_phone), invite);
        tokio::spawn(async move { bob_phone.pods().join(bob, invite).await })
    };
    pause.wait_until_reached().await;
    let offered_on = alice_phone.pod_membership_view_for_test(alice, pod).await?;
    assert_eq!(
        offered_on
            .member(&bob)
            .map(|member| (member.run(), member.state.member)),
        Some((1, true)),
        "the inviting device saw the leave before it made its offer"
    );
    alice_phone
        .refuse_pod_sessions_for_test(alice, false)
        .await?;
    pause.release();

    joining.await??;
    for (runtime, holder) in [(&alice_phone, alice), (&*bob_phone, bob)] {
        assert!(lists_members(runtime, holder, pod, both.clone()).await?);
    }
    let rejoined = bob_phone.pod_membership_view_for_test(bob, pod).await?;
    assert_eq!(
        rejoined.member(&bob).map(data_layer::Member::run),
        Some(3),
        "the joined event did not go past the leave"
    );
    bob_phone.refuse_pod_sessions_for_test(bob, false).await?;

    alice_phone.shutdown().await?;
    bob_phone.shutdown().await?;
    Ok(())
}

/// A pod with `owner` its creator on `owner_runtime` and `member` joined
/// from `member_runtime`.
async fn pod_of_two(
    owner_runtime: &Runtime,
    owner: PdnId,
    member_runtime: &Runtime,
    member: PdnId,
) -> Result<PodId> {
    let pod = owner_runtime.pods().create(owner).await?;
    let invite = owner_runtime.pods().invite(owner, pod, None).await?;
    member_runtime.pods().join(member, invite).await?;
    Ok(pod)
}

/// Whether `holder` on `runtime` comes to read `want` at `record`.
async fn reads(
    runtime: &Runtime,
    holder: PdnId,
    pod: PodId,
    record: RecordRef,
    want: &[u8],
) -> Result<bool> {
    eventually(|| async {
        Ok(runtime
            .pods()
            .read(holder, pod, record)
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
    pod: PodId,
    record: RecordRef,
) -> Result<Vec<(PdnId, u64, Vec<u8>)>> {
    let mut read: Vec<_> = runtime
        .pods()
        .read_ops(holder, pod, record)
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
    pod: PodId,
    record: RecordRef,
    mut want: Vec<(PdnId, u64, Vec<u8>)>,
) -> Result<bool> {
    want.sort();
    eventually(|| async {
        Ok(operations(runtime, holder, pod, record).await.ok().as_ref() == Some(&want))
    })
    .await
}

/// A claim and an immutable-document round-trip unchanged under the name of
/// the member that placed them, and a write addressed at either is refused,
/// by that member and by the owner alike, every member reading the bytes
/// placed first and listing no record beside the two. Denied: a co-located
/// identity that is no member reads nothing of the pod.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_and_an_immutable_document_are_placed_once() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let erin = alice_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;

    let mut placed = Vec::new();
    for kind in [RecordKind::Claim, RecordKind::ImmutableDocument] {
        let record = bob_phone
            .pods()
            .put_record(bob, pod, kind, b"placed first")
            .await?;
        assert_eq!((record.member, record.kind), (bob, kind));
        assert!(reads(&alice_phone, alice, pod, record, b"placed first").await?);
        for (runtime, writer) in [(&bob_phone, bob), (&alice_phone, alice)] {
            let refused = runtime
                .pods()
                .append_op(writer, pod, record, b"placed again")
                .await;
            assert!(refused.is_err_and(|err| is::<RecordPlacedOnce>(&err)));
            assert_eq!(
                runtime.pods().read(writer, pod, record).await?.as_deref(),
                Some(&b"placed first"[..])
            );
        }
        let refused = alice_phone.pods().read_ops(alice, pod, record).await;
        assert!(refused.is_err_and(|err| is::<WrongRecordKind>(&err)));
        // Denied (outsider).
        let refused = alice_phone.pods().read(erin, pod, record).await;
        assert!(refused.is_err_and(|err| is::<UnknownPod>(&err)));
        placed.push(record);
    }
    placed.sort();
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        assert_eq!(runtime.pods().list_records(holder, pod).await?, placed);
    }
    let refused = alice_phone.pods().list_records(erin, pod).await;
    assert!(refused.is_err_and(|err| is::<UnknownPod>(&err)));

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A plain member's operations on the owner's mergeable-document, three of
/// them appended at once, read on the owner's device as that member's, each
/// under an operation sequence of its own. Denied: a co-located identity that
/// is no member edits and reads nothing, and an operation addressed at a
/// mergeable-document the pod does not hold is refused, so none is created
/// under another member's name.
#[tokio::test(flavor = "multi_thread")]
async fn any_member_edits_another_members_mergeable_document() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let erin = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let note = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::MergeableDocument, b"milk")
        .await?;
    let first = vec![(alice, 1, b"milk".to_vec())];
    assert!(reads_ops(&bob_phone, bob, pod, note, first.clone()).await?);

    let pods = bob_phone.pods();
    let (one, two, three) = tokio::join!(
        pods.append_op(bob, pod, note, b"eggs"),
        pods.append_op(bob, pod, note, b"eggs"),
        pods.append_op(bob, pod, note, b"eggs"),
    );
    for appended in [one, two, three] {
        appended?;
    }
    let mut edited = first;
    edited.extend((1..=3).map(|op_seq| (bob, op_seq, b"eggs".to_vec())));
    assert!(reads_ops(&alice_phone, alice, pod, note, edited.clone()).await?);

    // Denied (outsider).
    let refused = pods.append_op(erin, pod, note, b"cake").await;
    assert!(refused.is_err_and(|err| is::<UnknownPod>(&err)));
    let refused = pods.read_ops(erin, pod, note).await;
    assert!(refused.is_err_and(|err| is::<UnknownPod>(&err)));
    // Denied: a record the pod does not hold.
    let absent = RecordRef {
        id: RecordId::from_bytes([0x77; 16]),
        ..note
    };
    let refused = pods.append_op(bob, pod, absent, b"cake").await;
    assert!(refused.is_err_and(|err| is::<UnknownRecord>(&err)));
    let refused = pods.read_ops(bob, pod, absent).await;
    assert!(refused.is_err_and(|err| is::<UnknownRecord>(&err)));
    // Sentinel: an operation appended after the refusals reaches the owner.
    pods.append_op(bob, pod, note, b"bread").await?;
    edited.push((bob, 4, b"bread".to_vec()));
    assert!(reads_ops(&alice_phone, alice, pod, note, edited).await?);
    assert_eq!(alice_phone.pods().list_records(alice, pod).await?, [note]);

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Three identities with no connection to one another share through one
/// pod: a claim one invited member places reads on the other's device, and
/// none of the three lists a connection. Denied: an identity on that
/// device's node holding a connection with the claim's author and no
/// membership lists no such pod and reads nothing of it.
#[tokio::test(flavor = "multi_thread")]
async fn three_identities_share_through_a_pod_with_no_connections() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let pod = pod_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    let claim = bob_phone
        .pods()
        .put_record(bob, pod, RecordKind::Claim, b"lease scan")
        .await?;
    assert!(reads(&carol_phone, carol, pod, claim, b"lease scan").await?);
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
    assert!(carol_phone.pods().list(erin).await?.is_empty());
    let refused = carol_phone.pods().read(erin, pod, claim).await;
    assert!(refused.is_err_and(|err| is::<UnknownPod>(&err)));

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Two pods with the same members keep their entries apart: a claim placed
/// in each reads there on the other member's device, and neither pod's
/// stores hold an entry of the other's claim. Denied: each claim addressed
/// at the other pod, by either member, reads nothing.
#[tokio::test(flavor = "multi_thread")]
async fn two_pods_with_the_same_members_keep_their_entries_apart() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let household = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let taxes = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let lease = bob_phone
        .pods()
        .put_record(bob, taxes, RecordKind::Claim, b"lease scan")
        .await?;
    let shopping = bob_phone
        .pods()
        .put_record(bob, household, RecordKind::Claim, b"shopping list")
        .await?;
    assert!(reads(&alice_phone, alice, taxes, lease, b"lease scan").await?);
    assert!(reads(&alice_phone, alice, household, shopping, b"shopping list").await?);

    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        for (pod, own) in [(taxes, lease), (household, shopping)] {
            assert_eq!(runtime.pods().list_records(holder, pod).await?, [own]);
            let view = runtime.pod_record_view_for_test(holder, pod).await?;
            let held: Vec<_> = view
                .verdicts()
                .map(|(entry, _)| RecordKey::parse(&entry.key).map(|key| key.record()))
                .collect();
            assert_eq!(held, [Some(own)]);
            assert!(runtime.pods().list_unknown(holder, pod).await?.is_empty());
        }
        // Denied: the other pod's claim.
        assert_eq!(runtime.pods().read(holder, household, lease).await?, None);
        assert_eq!(runtime.pods().read(holder, taxes, shopping).await?, None);
    }

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A pod `alice` created on `alice_phone`, `bob` and `carol` invited by her.
async fn pod_of_three(
    (alice_phone, alice): (&Runtime, PdnId),
    (bob_phone, bob): (&Runtime, PdnId),
    (carol_phone, carol): (&Runtime, PdnId),
) -> Result<PodId> {
    let pod = pod_of_two(alice_phone, alice, bob_phone, bob).await?;
    let invite = alice_phone.pods().invite(alice, pod, None).await?;
    carol_phone.pods().join(carol, invite).await?;
    Ok(pod)
}

fn refused(acted: Result<()>, reason: ActRefusal) -> bool {
    acted.is_err_and(|err| {
        err.downcast_ref::<ActRefused>()
            .is_some_and(|refusal| refusal.reason == reason)
    })
}

/// Whether `holder` on `runtime` comes to list `pod` among its pods, or
/// to list it no longer.
async fn lists_pod(runtime: &Runtime, holder: PdnId, pod: PodId, held: bool) -> Result<bool> {
    eventually(|| async {
        let listed = runtime.pods().list(holder).await?;
        Ok(listed.iter().any(|info| info.id == pod) == held)
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
    let pod = pod_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;

    // Denied (a plain member).
    let pods = carol_phone.pods();
    let promoted = pods.act(carol, pod, PodAct::Promote(carol)).await;
    assert!(refused(promoted, ActRefusal::NotAnOwner));
    let demoted = pods.act(carol, pod, PodAct::Demote(alice)).await;
    assert!(refused(demoted, ActRefusal::NotAnOwner));

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    let roles = vec![member(alice, true), member(bob, true), member(carol, false)];
    for (runtime, holder) in [
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    ] {
        assert!(lists_members(runtime, holder, pod, roles.clone()).await?);
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
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    let owners = vec![member(alice, true), member(bob, true)];
    assert!(lists_members(&bob_phone, bob, pod, owners.clone()).await?);

    // Denied: on itself.
    let demoted = alice_phone
        .pods()
        .act(alice, pod, PodAct::Demote(alice))
        .await;
    assert!(refused(demoted, ActRefusal::OnItself));
    assert_eq!(alice_phone.pods().members(alice, pod).await?, {
        let mut owners = owners;
        owners.sort();
        owners
    });

    bob_phone
        .pods()
        .act(bob, pod, PodAct::Demote(alice))
        .await?;
    let demoted = vec![member(alice, false), member(bob, true)];
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        assert!(lists_members(runtime, holder, pod, demoted.clone()).await?);
    }
    // Denied: a demoted owner, and a demotion of a plain member.
    let acted = alice_phone
        .pods()
        .act(alice, pod, PodAct::Demote(bob))
        .await;
    assert!(refused(acted, ActRefusal::NotAnOwner));
    let acted = bob_phone.pods().act(bob, pod, PodAct::Demote(alice)).await;
    assert!(refused(acted, ActRefusal::SubjectNotOwner));
    assert!(lists_members(&bob_phone, bob, pod, demoted).await?);

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// An owner removed by another owner learns of the removal and stops listing
/// the pod; invited again it joins as a plain member, every member listing
/// it so, until an owner promotes it anew and its demotion of that owner
/// goes through. Denied: the same demotion between the rejoin and the new
/// promotion.
#[tokio::test(flavor = "multi_thread")]
async fn a_former_owner_removed_and_invited_again_is_a_plain_member() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    let owners = vec![member(alice, true), member(bob, true)];
    assert!(lists_members(&bob_phone, bob, pod, owners).await?);

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Remove(bob))
        .await?;
    assert_eq!(
        alice_phone.pods().members(alice, pod).await?,
        [member(alice, true)]
    );
    assert!(
        lists_pod(&bob_phone, bob, pod, false).await?,
        "the removed owner's device did not learn of its removal"
    );
    let asked = bob_phone.pods().members(bob, pod).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));

    let invite = alice_phone.pods().invite(alice, pod, None).await?;
    bob_phone.pods().join(bob, invite).await?;
    let rejoined = vec![member(alice, true), member(bob, false)];
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        assert!(lists_members(runtime, holder, pod, rejoined.clone()).await?);
    }
    assert!(lists_pod(&bob_phone, bob, pod, true).await?);
    // Denied: an owner's act before an owner promotes it anew.
    let demoted = bob_phone.pods().act(bob, pod, PodAct::Demote(alice)).await;
    assert!(refused(demoted, ActRefusal::NotAnOwner));
    assert!(lists_members(&alice_phone, alice, pod, rejoined).await?);

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    let owners = vec![member(alice, true), member(bob, true)];
    assert!(lists_members(&bob_phone, bob, pod, owners).await?);
    bob_phone
        .pods()
        .act(bob, pod, PodAct::Demote(alice))
        .await?;
    let demoted = vec![member(alice, false), member(bob, true)];
    assert!(lists_members(&alice_phone, alice, pod, demoted).await?);

    for runtime in [alice_phone, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// An owner's removal of a plain member reaches the remaining member, the two
/// still syncing, and the removed member's device stops listing the pod.
/// Denied: the removed member's removal of the remaining one, refused while it
/// was a plain member, and the owner's removal of itself, the remaining member
/// still listed and served.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_removes_a_member_and_a_plain_member_removes_nobody() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let pod = pod_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;

    // Denied (a plain member, and on itself).
    let removed = carol_phone
        .pods()
        .act(carol, pod, PodAct::Remove(bob))
        .await;
    assert!(refused(removed, ActRefusal::NotAnOwner));
    let removed = alice_phone
        .pods()
        .act(alice, pod, PodAct::Remove(alice))
        .await;
    assert!(refused(removed, ActRefusal::OnItself));

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Remove(carol))
        .await?;
    let remaining = vec![member(alice, true), member(bob, false)];
    assert!(lists_members(&bob_phone, bob, pod, remaining).await?);
    assert!(
        lists_pod(&carol_phone, carol, pod, false).await?,
        "the removed member's device did not learn of its removal"
    );
    let claim = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"after the removal")
        .await?;
    assert!(reads(&bob_phone, bob, pod, claim, b"after the removal").await?);
    let asked = carol_phone.pods().read(carol, pod, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// The one owner leaves only once another member is an owner, the promotion
/// written just before the leave making that member the one owner on every
/// remaining device; the one member of a pod leaves as any member does.
/// Denied: the one owner's leave while the pod has other members.
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
    let pod = pod_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    let alone = alice_phone.pods().create(alice).await?;

    // Denied: the one owner.
    let left = alice_phone.pods().act(alice, pod, PodAct::Leave).await;
    assert!(refused(left, ActRefusal::SoleOwner));
    let mut all = vec![
        member(alice, true),
        member(bob, false),
        member(carol, false),
    ];
    all.sort();
    assert_eq!(alice_phone.pods().members(alice, pod).await?, all);

    let pods = alice_phone.pods();
    pods.act(alice, pod, PodAct::Promote(bob)).await?;
    pods.act(alice, pod, PodAct::Leave).await?;
    pods.act(alice, alone, PodAct::Leave).await?;
    assert!(pods.list(alice).await?.is_empty());
    let asked = pods.members(alice, pod).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));
    let left = vec![member(bob, true), member(carol, false)];
    for (runtime, holder) in [(&bob_phone, bob), (&carol_phone, carol)] {
        assert!(lists_members(runtime, holder, pod, left.clone()).await?);
    }

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A member that leaves right after writing stops listing the pod, and
/// the claim and the operation it wrote reach the remaining members; a
/// co-located member's leave then spares the member beside it. Denied: a
/// departed member reads nothing of the pod. The leave flushes to one of
/// the two members on the tablet, and the other takes what it wrote at the
/// tablet's pod pass, run every half second here.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_leaves_and_what_it_wrote_stays() -> Result<()> {
    let tablet = Runtime::spawn(SpawnOptions {
        pod_reconcile_interval: std::time::Duration::from_millis(500),
        ..SpawnOptions::memory()
    })
    .await?;
    let bob_phone = memory_runtime().await?;
    let leisure = tablet.identity().create().await?;
    let work = tablet.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&tablet, leisure, &bob_phone, bob).await?;
    let invite = tablet.pods().invite(leisure, pod, None).await?;
    tablet.pods().join(work, invite).await?;
    let note = tablet
        .pods()
        .put_record(leisure, pod, RecordKind::MergeableDocument, b"milk")
        .await?;
    let first = vec![(leisure, 1, b"milk".to_vec())];
    assert!(reads_ops(&bob_phone, bob, pod, note, first).await?);

    let pods = bob_phone.pods();
    let claim = pods
        .put_record(bob, pod, RecordKind::Claim, b"bob's claim")
        .await?;
    pods.append_op(bob, pod, note, b"eggs").await?;
    pods.act(bob, pod, PodAct::Leave).await?;
    assert!(pods.list(bob).await?.is_empty());
    // Denied: the departed member.
    let asked = pods.read(bob, pod, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));
    let edited = vec![(leisure, 1, b"milk".to_vec()), (bob, 1, b"eggs".to_vec())];
    let remaining = vec![member(leisure, true), member(work, false)];
    for holder in [leisure, work] {
        assert!(lists_members(&tablet, holder, pod, remaining.clone()).await?);
        assert!(reads(&tablet, holder, pod, claim, b"bob's claim").await?);
        assert!(reads_ops(&tablet, holder, pod, note, edited.clone()).await?);
    }

    tablet.pods().act(work, pod, PodAct::Leave).await?;
    assert!(tablet.pods().list(work).await?.is_empty());
    let asked = tablet.pods().read(work, pod, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));
    assert!(lists_members(&tablet, leisure, pod, vec![member(leisure, true)]).await?);
    assert!(lists_pod(&tablet, leisure, pod, true).await?);
    assert!(reads(&tablet, leisure, pod, claim, b"bob's claim").await?);

    for runtime in [tablet, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Devices linked into a member before and after its join reach the pod
/// from the identity's PMS, read it, and register themselves, so the
/// owner's device reads what they write and serves one of them with every
/// other device of the member gone. Denied: a co-located identity that is
/// no member lists no such pod and reads nothing of it.
#[tokio::test(flavor = "multi_thread")]
async fn devices_linked_before_and_after_the_join_reach_the_pod() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let (bob_laptop, bob_tablet) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let dave = bob_laptop.identity().create().await?;
    let pod = alice_phone.pods().create(alice).await?;
    let before = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"before")
        .await?;
    link_patiently(&bob_laptop, &bob_phone, bob).await?;
    let invite = alice_phone.pods().invite(alice, pod, None).await?;
    bob_phone.pods().join(bob, invite).await?;
    link_patiently(&bob_tablet, &bob_phone, bob).await?;

    for (device, said) in [(&bob_laptop, &b"laptop"[..]), (&bob_tablet, &b"tablet"[..])] {
        assert!(lists_pod(device, bob, pod, true).await?);
        assert!(reads(device, bob, pod, before, b"before").await?);
        let placed = device
            .pods()
            .put_record(bob, pod, RecordKind::Claim, said)
            .await?;
        assert!(
            reads(&alice_phone, alice, pod, placed, said).await?,
            "the owner's device did not read what a linked device wrote"
        );
    }
    // Denied (a co-located non-member).
    assert!(bob_laptop.pods().list(dave).await?.is_empty());
    let asked = bob_laptop.pods().read(dave, pod, before).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));

    bob_phone.shutdown().await?;
    bob_tablet.shutdown().await?;
    let after = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"after")
        .await?;
    assert!(
        reads(&bob_laptop, bob, pod, after, b"after").await?,
        "the owner's device did not serve the linked device"
    );

    for runtime in [alice_phone, bob_laptop] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A pod's tickets reach a device linked into the identity, each naming
/// the device it was minted on as the identity that holds the stores there:
/// a created pod's two under its own kinds, and a joined pod's two beside
/// the two the inviting device handed over, under the inviter's kinds.
#[tokio::test(flavor = "multi_thread")]
async fn a_pods_tickets_reach_a_linked_device_each_naming_its_holder() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
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
        let (probe, pms) = link_probe(runtime, holder).await?;
        for store in [PodStore::Membership, PodStore::Records] {
            let own = pod_ticket_kind(&pod, store);
            assert!(
                eventually(|| async {
                    Ok(pms
                        .get_ticket(&own)
                        .await?
                        .is_some_and(|ticket| names(&ticket, runtime, holder)))
                })
                .await?,
                "the pod's own ticket did not name its device as its holder"
            );
            let handed = pod_inviter_ticket_kind(&pod, store);
            match inviter {
                None => assert!(pms.get_ticket(&handed).await?.is_none()),
                Some((inviting, inviter)) => assert!(
                    eventually(|| async {
                        Ok(pms
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

/// A leave on one of a member's devices, and an owner's removal of the member,
/// each reach the member's other device, which stops listing the pod and
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
    let family = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let wedding = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let claims = [
        alice_phone
            .pods()
            .put_record(alice, family, RecordKind::Claim, b"family")
            .await?,
        alice_phone
            .pods()
            .put_record(alice, wedding, RecordKind::Claim, b"wedding")
            .await?,
    ];
    for (pod, claim, said) in [
        (family, claims[0], &b"family"[..]),
        (wedding, claims[1], &b"wedding"[..]),
    ] {
        assert!(reads(&bob_laptop, bob, pod, claim, said).await?);
    }

    bob_phone.pods().act(bob, family, PodAct::Leave).await?;
    alice_phone
        .pods()
        .act(alice, wedding, PodAct::Remove(bob))
        .await?;
    for (pod, claim) in [(family, claims[0]), (wedding, claims[1])] {
        for device in [&bob_phone, &bob_laptop] {
            assert!(
                lists_pod(device, bob, pod, false).await?,
                "a device of the departed member still lists the pod"
            );
            let asked = device.pods().read(bob, pod, claim).await;
            assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));
        }
        assert!(lists_members(&alice_phone, alice, pod, vec![member(alice, true)]).await?);
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

/// A member's runtime on a storage directory hosts its pod again after a
/// restart, from its PMS alone: the claim another member placed
/// meanwhile arrives, the member's next operation continues its author's
/// count, and its hosting record is the one its create wrote.
#[tokio::test(flavor = "multi_thread")]
async fn a_pod_is_hosted_again_after_a_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let alice_phone = memory_runtime().await?;
    let bob_tablet = runtime_on(dir.path()).await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_tablet.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_tablet, bob).await?;
    let note = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::MergeableDocument, b"milk")
        .await?;
    let mut edited = vec![(alice, 1, b"milk".to_vec())];
    assert!(reads_ops(&bob_tablet, bob, pod, note, edited.clone()).await?);
    for _ in 0..3 {
        bob_tablet.pods().append_op(bob, pod, note, b"eggs").await?;
    }
    edited.extend((1..=3).map(|op_seq| (bob, op_seq, b"eggs".to_vec())));
    assert!(reads_ops(&alice_phone, alice, pod, note, edited.clone()).await?);
    let record = dir
        .path()
        .join("identities")
        .join(bob.to_string())
        .join("pms");
    let recorded = std::fs::read(&record)?;
    bob_tablet.shutdown().await?;
    drop(bob_tablet);

    let meanwhile = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"meanwhile")
        .await?;
    let bob_tablet = runtime_on(dir.path()).await?;
    assert!(lists_pod(&bob_tablet, bob, pod, true).await?);
    assert!(reads(&bob_tablet, bob, pod, meanwhile, b"meanwhile").await?);
    bob_tablet
        .pods()
        .append_op(bob, pod, note, b"bread")
        .await?;
    edited.push((bob, 4, b"bread".to_vec()));
    assert!(reads_ops(&alice_phone, alice, pod, note, edited).await?);
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

/// A pod's creator restarted on its storage directory reads the claim a
/// member placed while it was down: none of its tickets names the member's
/// device, and its address book does. The wait is a sixth of the pod stores'
/// pass, so the pass is not what delivers it.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_creator_reaches_its_member_again() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let alice_tablet = runtime_on(dir.path()).await?;
    let bob_phone = memory_runtime().await?;
    let alice = alice_tablet.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_tablet, alice, &bob_phone, bob).await?;
    let before = bob_phone
        .pods()
        .put_record(bob, pod, RecordKind::Claim, b"before")
        .await?;
    assert!(reads(&alice_tablet, alice, pod, before, b"before").await?);
    alice_tablet.shutdown().await?;
    drop(alice_tablet);

    let meanwhile = bob_phone
        .pods()
        .put_record(bob, pod, RecordKind::Claim, b"meanwhile")
        .await?;
    let alice_tablet = runtime_on(dir.path()).await?;
    assert!(
        reads(&alice_tablet, alice, pod, meanwhile, b"meanwhile").await?,
        "the restarted creator never reached its member"
    );

    for runtime in [alice_tablet, bob_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Two members hosted on one node come back from a restart each with its
/// own copy of their pod, and a record one places reaches the other with no
/// other node reachable; a pod the second left before the restart stays
/// left, its membership store alone kept as the tombstone. Denied: the left
/// pod's records, to the member that left it.
#[tokio::test(flavor = "multi_thread")]
async fn co_located_members_come_back_and_a_left_pod_stays_left() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let tablet = runtime_on(dir.path()).await?;
    let leisure = tablet.identity().create().await?;
    let work = tablet.identity().create().await?;
    let wedding = tablet.pods().create(leisure).await?;
    let invite = tablet.pods().invite(leisure, wedding, None).await?;
    tablet.pods().join(work, invite).await?;
    let left = tablet.pods().create(work).await?;
    let before = tablet
        .pods()
        .put_record(work, left, RecordKind::Claim, b"before the leave")
        .await?;
    tablet.pods().act(work, left, PodAct::Leave).await?;
    tablet.shutdown().await?;
    drop(tablet);

    let tablet = runtime_on(dir.path()).await?;
    let both = vec![member(leisure, true), member(work, false)];
    for holder in [leisure, work] {
        assert!(lists_pod(&tablet, holder, wedding, true).await?);
        assert!(lists_members(&tablet, holder, wedding, both.clone()).await?);
    }
    let mut kept = vec![(wedding, true), (left, false)];
    kept.sort();
    assert!(
        eventually(|| async {
            let mut holdings = tablet.pod_holdings_for_test(work).await?;
            holdings.sort();
            Ok(holdings == kept)
        })
        .await?,
        "the left pod came back as more or less than its tombstone"
    );
    assert!(!tablet
        .pods()
        .list(work)
        .await?
        .iter()
        .any(|info| info.id == left));
    // Denied: the left pod.
    let asked = tablet.pods().read(work, left, before).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));

    let claim = tablet
        .pods()
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
    let pod = pod_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    let owners = vec![member(alice, true), member(bob, true), member(carol, false)];
    assert!(lists_members(&bob_phone, bob, pod, owners).await?);
    let earlier = bob_phone
        .pods()
        .put_record(bob, pod, RecordKind::Claim, b"before the leave")
        .await?;
    assert!(reads(&carol_phone, carol, pod, earlier, b"before the leave").await?);

    bob_phone.pods().act(bob, pod, PodAct::Leave).await?;
    assert!(lists_pod(&bob_phone, bob, pod, false).await?);
    let without = vec![member(alice, true), member(carol, false)];
    assert!(lists_members(&carol_phone, carol, pod, without).await?);
    let away = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"while away")
        .await?;

    let invite = alice_phone.pods().invite(alice, pod, None).await?;
    bob_phone.pods().join(bob, invite).await?;
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
        assert!(lists_members(runtime, holder, pod, rejoined.clone()).await?);
    }
    assert!(reads(&bob_phone, bob, pod, earlier, b"before the leave").await?);
    assert!(reads(&bob_phone, bob, pod, away, b"while away").await?);
    let later = bob_phone
        .pods()
        .put_record(bob, pod, RecordKind::Claim, b"after the return")
        .await?;
    for (runtime, holder) in [(&alice_phone, alice), (&carol_phone, carol)] {
        assert!(reads(runtime, holder, pod, later, b"after the return").await?);
    }
    // Denied: an owner's act before an owner promotes it anew.
    let promoted = bob_phone.pods().act(bob, pod, PodAct::Promote(carol)).await;
    assert!(refused(promoted, ActRefusal::NotAnOwner));

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    let promoted_anew = vec![member(alice, true), member(bob, true), member(carol, false)];
    assert!(lists_members(&bob_phone, bob, pod, promoted_anew).await?);
    bob_phone
        .pods()
        .act(bob, pod, PodAct::Promote(carol))
        .await?;
    let all_owners = vec![member(alice, true), member(bob, true), member(carol, true)];
    assert!(lists_members(&carol_phone, carol, pod, all_owners).await?);

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A member promoted, demoted and promoted again removes as its role at each
/// point allows, every member listing the roles its chain gives: its first
/// removal goes through, and its second once it is an owner again. Denied:
/// the second removal while it is a plain member.
#[allow(clippy::too_many_lines)] // one scenario: three role flips and a removal beside each
#[tokio::test(flavor = "multi_thread")]
async fn a_member_promoted_demoted_and_promoted_again_removes_as_its_role_allows() -> Result<()> {
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
    let pod = pod_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    let invite = alice_phone.pods().invite(alice, pod, None).await?;
    dave_phone.pods().join(dave, invite).await?;
    let owner_bob =
        |owner: bool| vec![member(alice, true), member(bob, owner), member(dave, false)];

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    assert!(
        lists_members(&bob_phone, bob, pod, {
            let mut four = owner_bob(true);
            four.push(member(carol, false));
            four
        })
        .await?
    );
    bob_phone
        .pods()
        .act(bob, pod, PodAct::Remove(carol))
        .await?;
    assert!(lists_members(&alice_phone, alice, pod, owner_bob(true)).await?);

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Demote(bob))
        .await?;
    assert!(lists_members(&bob_phone, bob, pod, owner_bob(false)).await?);
    // Denied (a plain member again).
    let removed = bob_phone.pods().act(bob, pod, PodAct::Remove(dave)).await;
    assert!(refused(removed, ActRefusal::NotAnOwner));
    assert!(lists_members(&dave_phone, dave, pod, owner_bob(false)).await?);

    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    assert!(lists_members(&bob_phone, bob, pod, owner_bob(true)).await?);
    bob_phone.pods().act(bob, pod, PodAct::Remove(dave)).await?;
    let remaining = vec![member(alice, true), member(bob, true)];
    for (runtime, holder) in [(&alice_phone, alice), (&bob_phone, bob)] {
        assert!(lists_members(runtime, holder, pod, remaining.clone()).await?);
    }
    let membership = alice_phone.pod_membership_view_for_test(alice, pod).await?;
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
/// sibling before the laptop's confirmation does, and is refused; its pod
/// pass, every half second here, opens the next one.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_linked_after_an_owners_demotion_lists_what_it_did_as_an_owner() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) = (
        memory_runtime().await?,
        memory_runtime().await?,
        memory_runtime().await?,
    );
    let alice_laptop = Runtime::spawn(SpawnOptions {
        pod_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    })
    .await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let carol = carol_phone.identity().create().await?;
    let pod = pod_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    alice_phone
        .pods()
        .act(alice, pod, PodAct::Promote(bob))
        .await?;
    let bob_owns = vec![member(alice, true), member(bob, true), member(carol, false)];
    assert!(lists_members(&bob_phone, bob, pod, bob_owns).await?);
    bob_phone
        .pods()
        .act(bob, pod, PodAct::Promote(carol))
        .await?;
    let all_own = vec![member(alice, true), member(bob, true), member(carol, true)];
    assert!(lists_members(&alice_phone, alice, pod, all_own).await?);
    alice_phone
        .pods()
        .act(alice, pod, PodAct::Demote(bob))
        .await?;
    let after = vec![member(alice, true), member(bob, false), member(carol, true)];
    assert!(lists_members(&bob_phone, bob, pod, after.clone()).await?);
    // Denied: the demoted owner.
    let promoted = bob_phone.pods().act(bob, pod, PodAct::Promote(bob)).await;
    assert!(refused(promoted, ActRefusal::NotAnOwner));

    link_patiently(&alice_laptop, &alice_phone, alice).await?;
    assert!(
        lists_members(&alice_laptop, alice, pod, after).await?,
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
    let pod = pod_of_three(
        (&alice_phone, alice),
        (&bob_phone, bob),
        (&carol_phone, carol),
    )
    .await?;
    let note = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::MergeableDocument, b"milk")
        .await?;
    let first = vec![(alice, 1, b"milk".to_vec())];
    for (runtime, holder) in [(&bob_phone, bob), (&carol_phone, carol)] {
        assert!(reads_ops(runtime, holder, pod, note, first.clone()).await?);
    }

    let (bobs, carols) = (bob_phone.pods(), carol_phone.pods());
    let (eggs, bread) = tokio::join!(
        bobs.append_op(bob, pod, note, b"eggs"),
        carols.append_op(carol, pod, note, b"bread"),
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
        assert!(reads_ops(runtime, holder, pod, note, edited.clone()).await?);
    }
    // Denied (a co-located non-member).
    let refused = carol_phone.pods().append_op(erin, pod, note, b"cake").await;
    assert!(refused.is_err_and(|err| is::<UnknownPod>(&err)));
    assert!(reads_ops(&alice_phone, alice, pod, note, edited).await?);

    for runtime in [alice_phone, bob_phone, carol_phone] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// Whether `holder`'s replica of `pod` on `runtime` comes to hold an entry
/// of `record` that reads nothing.
async fn holds_unread(
    runtime: &Runtime,
    holder: PdnId,
    pod: PodId,
    record: RecordRef,
) -> Result<bool> {
    eventually(|| async {
        let view = runtime.pod_record_view_for_test(holder, pod).await?;
        let held = view.verdicts().any(|(entry, verdict)| {
            verdict != Verdict::Counted
                && RecordKey::parse(&entry.key).is_some_and(|key| key.record() == record)
        });
        Ok(held)
    })
    .await
}

/// A linked device whose device statement did not land in one of its
/// member's two pods before a restart writes it after the restart, and
/// another member reads what the device placed there. Paired denial:
/// before the restart that member holds the device's claim in that pod
/// and reads nothing of it, while it reads the device's claim in the pod
/// the statement reached. A statement write failing in the one pod stands
/// in for a process ended between the two writes.
#[allow(clippy::too_many_lines)] // one scenario: the cut fan-out, the restart and the pod it heals
#[tokio::test(flavor = "multi_thread")]
async fn a_fan_out_cut_by_a_restart_is_healed_by_the_sweep() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let quick = || SpawnOptions {
        pod_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    };
    let (alice_phone, bob_phone) = (
        Runtime::spawn(quick()).await?,
        Runtime::spawn(quick()).await?,
    );
    let on_dir = || SpawnOptions {
        pod_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::on_directory(dir.path())
    };
    let bob_laptop = Runtime::spawn(on_dir()).await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let family = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let wedding = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;

    bob_laptop.fail_device_statements_for_test(wedding).await;
    link_patiently(&bob_laptop, &bob_phone, bob).await?;
    let mut placed = Vec::new();
    let both = vec![member(alice, true), member(bob, false)];
    for (pod, said) in [(family, &b"family"[..]), (wedding, &b"wedding"[..])] {
        assert!(lists_members(&bob_laptop, bob, pod, both.clone()).await?);
        let record = bob_laptop
            .pods()
            .put_record(bob, pod, RecordKind::Claim, said)
            .await?;
        placed.push((pod, record, said));
    }
    let [(_, in_family, _), (_, in_wedding, _)] = placed.as_slice() else {
        anyhow::bail!("two claims placed, {} listed", placed.len());
    };
    assert!(reads(&alice_phone, alice, family, *in_family, b"family").await?);
    // Denied: the device no statement in the pod lists.
    assert!(holds_unread(&alice_phone, alice, wedding, *in_wedding).await?);
    let asked = alice_phone.pods().read(alice, wedding, *in_wedding).await?;
    assert!(asked.is_none());

    bob_laptop.shutdown().await?;
    drop(bob_laptop);
    let bob_laptop = Runtime::spawn(on_dir()).await?;
    for (pod, record, said) in &placed {
        assert!(
            reads(&alice_phone, alice, *pod, *record, said).await?,
            "the sweep after the restart did not list the device"
        );
    }

    for runtime in [alice_phone, bob_phone, bob_laptop] {
        runtime.shutdown().await?;
    }
    Ok(())
}

/// A device linked into a member after its leave takes the pod's tombstone
/// from its sibling: it holds the membership store alone, its membership
/// view showing its member there as no member, and lists no such pod.
/// Denied: it reads nothing of the pod. A tombstone, out of the swarm, is
/// reconciled by the pod pass alone, which runs every half second on the
/// laptop here.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_linked_after_the_departure_takes_the_tombstone_from_a_sibling() -> Result<()> {
    let (alice_phone, bob_phone) = (memory_runtime().await?, memory_runtime().await?);
    let bob_laptop = Runtime::spawn(SpawnOptions {
        pod_reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    })
    .await?;
    let alice = alice_phone.identity().create().await?;
    let bob = bob_phone.identity().create().await?;
    let pod = pod_of_two(&alice_phone, alice, &bob_phone, bob).await?;
    let claim = alice_phone
        .pods()
        .put_record(alice, pod, RecordKind::Claim, b"before the leave")
        .await?;
    assert!(reads(&bob_phone, bob, pod, claim, b"before the leave").await?);
    bob_phone.pods().act(bob, pod, PodAct::Leave).await?;

    link_patiently(&bob_laptop, &bob_phone, bob).await?;
    let left = MemberState {
        member: false,
        owner: false,
    };
    assert!(
        eventually(|| async {
            let holdings = bob_laptop.pod_holdings_for_test(bob).await?;
            let view = bob_laptop.pod_membership_view_for_test(bob, pod).await;
            Ok(holdings == [(pod, false)]
                && view.is_ok_and(|membership| {
                    membership.member(&bob).map(|bob| bob.state) == Some(left)
                }))
        })
        .await?,
        "the linked device did not take the tombstone from its sibling"
    );
    assert!(bob_laptop.pods().list(bob).await?.is_empty());
    // Denied: the departed member's new device.
    let asked = bob_laptop.pods().read(bob, pod, claim).await;
    assert!(asked.is_err_and(|err| is::<UnknownPod>(&err)));

    for runtime in [alice_phone, bob_phone, bob_laptop] {
        runtime.shutdown().await?;
    }
    Ok(())
}
