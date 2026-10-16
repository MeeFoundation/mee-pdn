//! Sessions on a pod's stores, served by the member the caller
//! names: a sibling device by its identity's own PMS, another member's
//! device by that member's device statements, and every other caller refused
//! as for an unhosted replica. The entries a pod's creation and its joins
//! write arrive by the store-level writes the pods service performs, and
//! the tickets by hand. The reconcile pass is set out of reach and the
//! sessions a scenario asserts on are opened by name; where a refusal
//! depends on what a device has not received, the devices it had sessions
//! with are settled first, since out of a swarm it is still dialed by them.

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    identity_of, AddrInfoOptions, Contact, EventKind, IdentityKeyPair, MemberDevice, MemberState,
    MembershipKey, NamespaceId, PodStore, PodTickets, PrivateMetadataStore, Seq, SpawnOptions,
    SyncNode, Verdict,
};
use pdn_types::{PdnId, PodId};
use test_utils::{eventually, host_identity, join_identity, wait_devices, TIMEOUT};

/// Out of every scenario's reach: no pass opens a session a scenario did
/// not name.
const QUIET: Duration = Duration::from_secs(3600);

const PLAIN: MemberState = MemberState {
    member: true,
    owner: false,
};
const OWNER: MemberState = MemberState {
    member: true,
    owner: true,
};

async fn node(reconcile_interval: Duration) -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval,
        ..SpawnOptions::memory()
    })
    .await
}

/// An identity whose `PdnId` derives from the identity key pair beside it.
struct Identity {
    keys: IdentityKeyPair,
    id: PdnId,
}

async fn host(node: &SyncNode) -> Result<(Identity, PrivateMetadataStore)> {
    let keys = IdentityKeyPair::generate();
    let id = keys.pdn_id();
    let pms = host_identity(node, id).await?;
    Ok((Identity { keys, id }, pms))
}

fn device_of(node: &SyncNode, identity: &Identity) -> Result<MemberDevice> {
    Ok(MemberDevice {
        node: node.node_id(),
        author: node.default_author(identity.id)?,
    })
}

async fn write(
    node: &SyncNode,
    writer: &Identity,
    pod: PodId,
    key: MembershipKey,
    payload: Vec<u8>,
) -> Result<()> {
    node.write_pod_entry(
        writer.id,
        pod,
        PodStore::Membership,
        &key.to_bytes(),
        &payload,
    )
    .await
}

async fn statement(
    node: &SyncNode,
    writer: &Identity,
    pod: PodId,
    member: &Identity,
    version: u64,
    devices: Vec<MemberDevice>,
) -> Result<()> {
    let key = MembershipKey::Devices {
        member: member.id,
        version,
    };
    let payload = member
        .keys
        .device_statement(&pod, version, devices)
        .encode();
    write(node, writer, pod, key, payload).await
}

/// `creator`'s pod on `node`: both stores, the created event, the
/// creator's first device statement, and the tickets to both stores.
async fn create(node: &SyncNode, creator: &Identity) -> Result<(PodId, PodTickets)> {
    let pod = data_layer::pod_id_of(&creator.id, &creator.keys.public_key(), &[0x5a; 16]);
    node.create_pod(creator.id, pod).await?;
    let (membership_store, record_store) = node.pod_stores(creator.id, pod)?;
    let creation = creator
        .keys
        .creation([0x5a; 16], membership_store, record_store);
    write(
        node,
        creator,
        pod,
        MembershipKey::created(creator.id),
        creation.encode(),
    )
    .await?;
    statement(
        node,
        creator,
        pod,
        creator,
        1,
        vec![device_of(node, creator)?],
    )
    .await?;
    let tickets = node
        .share_pod_tickets(creator.id, pod, AddrInfoOptions::Addresses)
        .await?;
    Ok((pod, tickets))
}

/// The invite act for `newcomer` at its sequence 1, naming `inviter`'s
/// `inviter_seq`, its join statement naming `newcomer_device`, written on the
/// inviter's node as the join dialogue writes it.
async fn invite(
    node: &SyncNode,
    inviter: &Identity,
    inviter_seq: u64,
    pod: PodId,
    newcomer: &Identity,
    newcomer_device: MemberDevice,
) -> Result<()> {
    let key = MembershipKey::Event {
        subject: newcomer.id,
        seq: Seq::new(1),
        kind: EventKind::Joined,
        actor: inviter.id,
        actor_seq: Seq::new(inviter_seq),
    };
    let join_statement = newcomer
        .keys
        .join_statement(&pod, Seq::new(1), newcomer_device);
    write(node, inviter, pod, key, join_statement.encode()).await
}

/// A session `from` opens with `to` on `namespace`, from the replica of
/// `local`, addressing `to`'s replica of `addressed` and naming `caller`.
async fn session(
    from: &SyncNode,
    local: PdnId,
    namespace: NamespaceId,
    to: &SyncNode,
    addressed: PdnId,
    caller: PdnId,
) -> Result<()> {
    let contact = Contact::new(to.dial_handle().addr(), identity_of(addressed));
    from.sync_namespace_as_for_test(local, namespace, contact, identity_of(caller))
        .await
}

/// The uniform refusal, the one an unhosted replica answers with.
fn refused(result: Result<()>) -> bool {
    result.is_err_and(|err| format!("{err:#}").contains("NotFound"))
}

/// Whether `pod`'s stores come to have nothing in flight on every one of
/// `nodes` — no exchange running, held or due to redial — in two reads in
/// a row: a dial one node still makes lands on another between two reads.
async fn settle(nodes: &[&SyncNode], pod: PodId) -> Result<bool> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut quiet_reads = 0_u8;
    while std::time::Instant::now() < deadline {
        let mut in_flight = 0_usize;
        for node in nodes {
            in_flight = in_flight.saturating_add(node.pod_syncs_in_flight_for_test(pod).await?);
        }
        quiet_reads = if in_flight == 0 {
            quiet_reads.saturating_add(1)
        } else {
            0
        };
        if quiet_reads == 2 {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(false)
}

async fn state_on(node: &SyncNode, holder: PdnId, pod: PodId, member: PdnId) -> MemberState {
    match node.pod_membership_view(holder, pod).await {
        Ok(membership) => membership
            .member(&member)
            .map(|member| member.state)
            .unwrap_or_default(),
        Err(_unknown) => MemberState::default(),
    }
}

/// A member's device is served both stores and builds the pod's membership
/// view from nothing.
/// Denied: the callee of its first dial, which it does not yet resolve to a
/// member; a holder of both tickets that is no member, on either store; and
/// the member itself once removed, on the record store.
#[allow(clippy::too_many_lines)] // one scenario: the served member beside each denial
#[tokio::test(flavor = "multi_thread")]
async fn a_member_device_is_served_and_a_ticket_holder_is_not() -> Result<()> {
    let (alice_phone, bob_phone, dave_phone) =
        (node(QUIET).await?, node(QUIET).await?, node(QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (dave, _) = host(&dave_phone).await?;
    let (pod, tickets) = create(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        1,
        pod,
        &bob,
        device_of(&bob_phone, &bob)?,
    )
    .await?;
    let (membership, records) = (
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    );
    let mut verdicts = alice_phone
        .take_pod_verdicts()
        .expect("the verdict channel is taken once");
    // Refused while both import, so the imports' own dials bring them
    // nothing; Bob's phone out of both swarms and every device settled, so
    // the dials below are its only sessions.
    alice_phone.refuse_pod_sessions_for_test(alice.id, true)?;
    bob_phone.import_pod(bob.id, pod, tickets.clone()).await?;
    dave_phone.import_pod(dave.id, pod, tickets).await?;
    for namespace in [membership, records] {
        bob_phone.leave_swarm_for_test(bob.id, namespace).await?;
    }
    assert!(settle(&[&alice_phone, &bob_phone, &dave_phone], pod).await?);
    alice_phone.refuse_pod_sessions_for_test(alice.id, false)?;

    // A plain member's promotion of itself, carried to Alice's phone by the
    // second of the dials that follow.
    let own_promotion = MembershipKey::Event {
        subject: bob.id,
        seq: Seq::new(2),
        kind: EventKind::Promoted,
        actor: bob.id,
        actor_seq: Seq::new(1),
    };
    write(&bob_phone, &bob, pod, own_promotion, vec![0]).await?;
    let to_alice = Contact::new(alice_phone.dial_handle().addr(), identity_of(alice.id));
    let mut dials = bob_phone
        .watch_pod_sessions(bob.id, pod, PodStore::Membership)
        .await?;
    bob_phone
        .sync_pod_with_for_test(bob.id, pod, PodStore::Membership, to_alice.clone())
        .await?;
    assert!(
        dials
            .next_served_with(alice_phone.node_id(), TIMEOUT)
            .await?
            .is_some(),
        "the member's first dial did not go through"
    );
    assert!(
        eventually(|| async { Ok(state_on(&bob_phone, bob.id, pod, alice.id).await == OWNER) })
            .await?,
        "the member's device did not list the pod's creator after its first session"
    );
    // A joined event counts once its payload lands, which can trail the
    // created event's.
    assert!(
        eventually(|| async { Ok(state_on(&bob_phone, bob.id, pod, bob.id).await == PLAIN) })
            .await?,
        "the member's device did not count its own joined event"
    );
    // Denied: the callee of a dial whose member the dialer has not resolved
    // yet takes nothing from it.
    let promotion_key = own_promotion.to_bytes();
    alice_phone.pod_membership_view(alice.id, pod).await?;
    let mut held_early = false;
    while let Ok(report) = verdicts.try_recv() {
        held_early |= report
            .verdicts
            .iter()
            .any(|(key, _author, _verdict)| *key == promotion_key);
    }
    assert!(
        !held_early,
        "the first dial served the callee before the dialer resolved its member"
    );
    bob_phone
        .sync_pod_with_for_test(bob.id, pod, PodStore::Membership, to_alice)
        .await?;
    let judged = tokio::time::timeout(TIMEOUT, async {
        loop {
            alice_phone.pod_membership_view(alice.id, pod).await?;
            while let Ok(report) = verdicts.try_recv() {
                let found = report
                    .verdicts
                    .into_iter()
                    .find(|(key, _author, _verdict)| *key == promotion_key);
                if let Some((_key, _author, verdict)) = found {
                    return anyhow::Ok(verdict);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    assert!(
        matches!(judged, Verdict::CountedForNothing(_)),
        "the plain member's promotion of itself counted: {judged:?}"
    );
    assert_eq!(state_on(&alice_phone, alice.id, pod, bob.id).await, PLAIN);

    // Denied: the ticket holder that is no member.
    assert!(refused(
        session(
            &dave_phone,
            dave.id,
            membership,
            &alice_phone,
            alice.id,
            dave.id
        )
        .await
    ));
    assert_eq!(
        dave_phone
            .pod_membership_view(dave.id, pod)
            .await?
            .identities()
            .count(),
        0,
        "the refused ticket holder took entries"
    );
    session(&bob_phone, bob.id, records, &alice_phone, alice.id, bob.id).await?;
    // Denied: the ticket holder, on the record store.
    assert!(refused(
        session(
            &dave_phone,
            dave.id,
            records,
            &alice_phone,
            alice.id,
            dave.id
        )
        .await
    ));
    // Denied: the member once removed, on the record store; the membership
    // store is served to it over the removal's past.
    let removal = MembershipKey::Event {
        subject: bob.id,
        seq: Seq::new(3),
        kind: EventKind::Removed,
        actor: alice.id,
        actor_seq: Seq::new(1),
    };
    write(&alice_phone, &alice, pod, removal, vec![0]).await?;
    assert!(refused(
        session(&bob_phone, bob.id, records, &alice_phone, alice.id, bob.id).await
    ));
    session(
        &bob_phone,
        bob.id,
        membership,
        &alice_phone,
        alice.id,
        bob.id,
    )
    .await?;

    for node in [alice_phone, bob_phone, dave_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A session from a member's node naming that member is served, and the same
/// node naming a co-located identity that is no member is refused, though
/// both share the node's id.
#[tokio::test(flavor = "multi_thread")]
async fn a_co_located_non_member_is_refused_where_its_node_mate_is_served() -> Result<()> {
    let (alice_phone, tablet) = (node(QUIET).await?, node(QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&tablet).await?;
    let (erin, _) = host(&tablet).await?;
    let (pod, tickets) = create(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        1,
        pod,
        &bob,
        device_of(&tablet, &bob)?,
    )
    .await?;
    let membership = tickets.membership.capability.id();
    tablet.import_pod(bob.id, pod, tickets).await?;

    // Denied: the co-located non-member, through the member's own replica.
    assert!(refused(
        session(&tablet, bob.id, membership, &alice_phone, alice.id, erin.id).await
    ));
    session(&tablet, bob.id, membership, &alice_phone, alice.id, bob.id).await?;

    alice_phone.shutdown().await?;
    tablet.shutdown().await?;
    Ok(())
}

/// A freshly linked device is served by its sibling at once, by its
/// identity's PMS, and by another member once the statement it wrote
/// reaches that member. Paired denials: the same device refused the record
/// store by the other member before that statement exists, and by the
/// sibling a device naming the identity that the identity's PMS does not
/// list.
#[allow(clippy::too_many_lines)] // one scenario: the linking, the refusal and the statement that ends it
#[tokio::test(flavor = "multi_thread")]
async fn a_freshly_linked_device_is_served_by_its_sibling_first() -> Result<()> {
    let (alice_phone, bob_phone, bob_laptop, dave_phone) = (
        node(QUIET).await?,
        node(QUIET).await?,
        node(QUIET).await?,
        node(QUIET).await?,
    );
    let (alice, _) = host(&alice_phone).await?;
    let (bob, bob_pms) = host(&bob_phone).await?;
    let (dave, _) = host(&dave_phone).await?;
    let (pod, tickets) = create(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        1,
        pod,
        &bob,
        device_of(&bob_phone, &bob)?,
    )
    .await?;
    let membership = tickets.membership.capability.id();
    let records = tickets.records.capability.id();
    bob_phone.import_pod(bob.id, pod, tickets.clone()).await?;
    session(
        &bob_phone,
        bob.id,
        membership,
        &alice_phone,
        alice.id,
        bob.id,
    )
    .await?;
    assert!(
        eventually(|| async { Ok(state_on(&bob_phone, bob.id, pod, alice.id).await == OWNER) })
            .await?
    );

    bob_pms.add_device(bob_laptop.node_id()).await?;
    let ticket = bob_pms
        .share_ticket(data_layer::ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let laptop_pms = join_identity(&bob_laptop, bob.id, ticket).await?;
    assert!(
        wait_devices(&laptop_pms, &[bob_phone.node_id(), bob_laptop.node_id()]).await?,
        "the laptop's PMS did not list both devices"
    );
    bob_laptop.import_pod(bob.id, pod, tickets.clone()).await?;
    dave_phone.import_pod(dave.id, pod, tickets).await?;

    // Denied: the laptop, before any statement lists it.
    assert!(refused(
        session(&bob_laptop, bob.id, records, &alice_phone, alice.id, bob.id).await
    ));
    session(&bob_laptop, bob.id, membership, &bob_phone, bob.id, bob.id).await?;
    // Denied: a device naming Bob that Bob's PMS does not list.
    assert!(refused(
        session(&dave_phone, dave.id, membership, &bob_phone, bob.id, bob.id).await
    ));

    // The laptop's own statement: the phone and itself, under Bob's key.
    let devices = vec![device_of(&bob_phone, &bob)?, device_of(&bob_laptop, &bob)?];
    statement(&bob_laptop, &bob, pod, &bob, 2, devices).await?;
    session(&bob_laptop, bob.id, membership, &bob_phone, bob.id, bob.id).await?;
    session(
        &bob_phone,
        bob.id,
        membership,
        &alice_phone,
        alice.id,
        bob.id,
    )
    .await?;
    assert!(
        eventually(|| async {
            Ok(
                session(&bob_laptop, bob.id, records, &alice_phone, alice.id, bob.id)
                    .await
                    .is_ok(),
            )
        })
        .await?,
        "the other member never served the laptop its statement lists"
    );

    for node in [alice_phone, bob_phone, bob_laptop, dave_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A newcomer is refused by a member device its joined event has not
/// reached, and served once the event arrives there: without an anchored
/// log the serving device cannot tell it from a stranger.
#[allow(clippy::too_many_lines)] // one scenario: the refusal and the arrival that ends it
#[tokio::test(flavor = "multi_thread")]
async fn wrongly_refused_without_anchoring_d19() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) =
        (node(QUIET).await?, node(QUIET).await?, node(QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let (pod, tickets) = create(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        1,
        pod,
        &bob,
        device_of(&bob_phone, &bob)?,
    )
    .await?;
    let membership = tickets.membership.capability.id();
    bob_phone.import_pod(bob.id, pod, tickets.clone()).await?;
    session(
        &bob_phone,
        bob.id,
        membership,
        &alice_phone,
        alice.id,
        bob.id,
    )
    .await?;
    assert!(
        eventually(|| async { Ok(state_on(&bob_phone, bob.id, pod, bob.id).await == PLAIN) })
            .await?
    );
    // Out of the swarm and settled, so Carol's joined event reaches Alice's
    // phone only by the session named below.
    bob_phone.leave_swarm_for_test(bob.id, membership).await?;
    assert!(settle(&[&alice_phone, &bob_phone], pod).await?);
    invite(
        &bob_phone,
        &bob,
        1,
        pod,
        &carol,
        device_of(&carol_phone, &carol)?,
    )
    .await?;
    carol_phone.import_pod(carol.id, pod, tickets).await?;

    // Denied: Carol, before her joined event reaches Alice's phone.
    assert!(refused(
        session(
            &carol_phone,
            carol.id,
            membership,
            &alice_phone,
            alice.id,
            carol.id
        )
        .await
    ));
    session(
        &bob_phone,
        bob.id,
        membership,
        &alice_phone,
        alice.id,
        bob.id,
    )
    .await?;
    assert!(
        eventually(|| async {
            Ok(session(
                &carol_phone,
                carol.id,
                membership,
                &alice_phone,
                alice.id,
                carol.id,
            )
            .await
            .is_ok())
        })
        .await?,
        "the newcomer was not served once its joined event arrived"
    );
    assert!(
        eventually(|| async { Ok(state_on(&carol_phone, carol.id, pod, carol.id).await == PLAIN) })
            .await?
    );

    for node in [alice_phone, bob_phone, carol_phone] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Two members hosted on one node converge on the membership store inside
/// the process with no other node reachable. Denied: a co-located identity
/// holding both tickets that is no member takes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn co_located_members_converge_and_a_co_located_ticket_holder_takes_nothing() -> Result<()> {
    let tablet = node(Duration::from_millis(200)).await?;
    let (bob, _) = host(&tablet).await?;
    let (dave, _) = host(&tablet).await?;
    let (erin, _) = host(&tablet).await?;
    let (pod, tickets) = create(&tablet, &bob).await?;
    invite(&tablet, &bob, 1, pod, &dave, device_of(&tablet, &dave)?).await?;
    tablet.import_pod(dave.id, pod, tickets.clone()).await?;
    tablet.import_pod(erin.id, pod, tickets).await?;

    assert!(
        eventually(|| async {
            Ok(state_on(&tablet, dave.id, pod, bob.id).await == OWNER
                && state_on(&tablet, dave.id, pod, dave.id).await == PLAIN)
        })
        .await?,
        "the co-located member did not converge"
    );
    // Sentinel: the passes that brought Dave the store reached Erin's pairs too.
    assert_eq!(
        tablet
            .pod_membership_view(erin.id, pod)
            .await?
            .identities()
            .count(),
        0,
        "the co-located ticket holder took entries"
    );

    tablet.shutdown().await?;
    Ok(())
}

/// A pod pass short enough that a refusal is waited out over several runs.
const POD_RUN: Duration = Duration::from_millis(200);

async fn pod_pass_node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: QUIET,
        pod_reconcile_interval: POD_RUN,
        ..SpawnOptions::memory()
    })
    .await
}

/// A device its identity's PMS lists as pending is served nothing of a
/// pod's record store by its sibling, while a confirmed device of the
/// identity is served the store whole, and the pending device is served
/// once it is confirmed.
///
/// Denied: the pending device takes nothing over several of its pod passes,
/// ordered after the confirmed device has taken the record.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_device_registration_confers_nothing() -> Result<()> {
    use test_utils::pod as c;

    let phone = pod_pass_node().await?;
    let tablet = pod_pass_node().await?;
    let laptop = pod_pass_node().await?;
    let (bob, phone_pms) = c::host(&phone).await?;
    let pod = c::create(&phone, &bob).await?;
    let record = c::place_claim(&phone, &bob, pod, 0x01).await?;
    let pms_ticket = phone_pms
        .share_ticket(data_layer::ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let _tablet_pms = join_identity(&tablet, bob.id, pms_ticket.clone()).await?;
    let _laptop_pms = join_identity(&laptop, bob.id, pms_ticket).await?;
    phone_pms.add_device(tablet.node_id()).await?;
    // The registration a linking dialogue leaves on the inviter.
    phone_pms.add_pending_device(laptop.node_id()).await?;
    let tickets = c::tickets(&phone, &bob, pod).await?;
    for device in [&tablet, &laptop] {
        device.import_pod(bob.id, pod, tickets.clone()).await?;
    }

    assert!(
        c::reads(&tablet, bob.id, pod, record).await?,
        "a confirmed device of the identity was not served the record store"
    );
    // Denied: pending is not a device, waited out over several pod passes.
    tokio::time::sleep(POD_RUN * 5).await;
    assert_eq!(
        c::held_records(&laptop, bob.id, pod).await?,
        Vec::<String>::new(),
        "a pending registration was served the record store"
    );

    phone_pms.confirm_device(laptop.node_id()).await?;
    assert!(
        c::reads(&laptop, bob.id, pod, record).await?,
        "the device was not served once confirmed"
    );

    for node in [phone, tablet, laptop] {
        node.shutdown().await?;
    }
    Ok(())
}

/// A caller is admitted for the member it names only where that member's
/// device statement lists the caller's node: Alice's own node, which her
/// join statement names, is served a pod's record store, and a node that
/// names her identity without being one of her devices is refused as if
/// the replica were not hosted.
///
/// Denied: the impersonating node's session is refused and its replica of
/// the record store stays empty, beside the identical session from Alice's
/// node that succeeds. Both sessions name the identities themselves,
/// through the fixture that lets a caller choose them.
#[tokio::test(flavor = "multi_thread")]
async fn a_caller_is_admitted_only_where_the_named_identity_lists_its_node() -> Result<()> {
    use test_utils::pod as c;

    let bob_phone = node(QUIET).await?;
    let alice_phone = node(QUIET).await?;
    let mallory = node(QUIET).await?;
    let (bob, _) = c::host(&bob_phone).await?;
    let (alice, _) = c::host(&alice_phone).await?;
    let (dave, _) = c::host(&mallory).await?;
    let pod = c::create(&bob_phone, &bob).await?;
    c::invite(
        &bob_phone,
        &bob,
        pod,
        &alice,
        vec![c::device_of(&alice_phone, &alice)?],
    )
    .await?;
    let record = c::place_claim(&bob_phone, &bob, pod, 0x01).await?;
    let tickets = c::tickets(&bob_phone, &bob, pod).await?;
    let records = tickets.records.capability.id();
    alice_phone
        .import_pod(alice.id, pod, tickets.clone())
        .await?;
    mallory.import_pod(dave.id, pod, tickets).await?;

    let toward_bob = Contact::new(bob_phone.dial_handle().addr(), identity_of(bob.id));
    alice_phone
        .sync_namespace_as_for_test(alice.id, records, toward_bob.clone(), identity_of(alice.id))
        .await?;
    assert!(
        c::reads(&alice_phone, alice.id, pod, record).await?,
        "a device of the named member must be served"
    );

    // Denied: a node Alice's statements do not list, claiming her identity.
    let refused = mallory
        .sync_namespace_as_for_test(dave.id, records, toward_bob, identity_of(alice.id))
        .await
        .expect_err("a caller the named member's statements do not list must be refused");
    assert!(
        format!("{refused:#}").contains("NotFound"),
        "the refusal must be the not-hosted abort, got: {refused:#}"
    );
    assert_eq!(
        c::held_records(&mallory, dave.id, pod).await?,
        Vec::<String>::new(),
        "an impersonating caller obtained records"
    );

    for node in [bob_phone, alice_phone, mallory] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Three refusals are one answer: a session for a namespace the node does
/// not host, one naming an identity the node does not host, and one from a
/// caller that is no member of the pod whose store it names are each
/// aborted as not found, which the codec writes as one frame
/// (`a_refusal_is_six_bytes_on_the_wire` in pdn-store's `net/codec.rs`).
///
/// Allowed beside them: the member's own session on the same store.
#[tokio::test(flavor = "multi_thread")]
async fn every_refusal_is_the_frame_an_unhosted_replica_produces() -> Result<()> {
    use test_utils::pod as c;

    let bob_phone = node(QUIET).await?;
    let dialer = node(QUIET).await?;
    let (bob, _) = c::host(&bob_phone).await?;
    let (alice, _) = c::host(&dialer).await?;
    let (dave, _) = c::host(&dialer).await?;
    let pod = c::create(&bob_phone, &bob).await?;
    c::invite(
        &bob_phone,
        &bob,
        pod,
        &alice,
        vec![c::device_of(&dialer, &alice)?],
    )
    .await?;
    let tickets = c::tickets(&bob_phone, &bob, pod).await?;
    let records = tickets.records.capability.id();
    dialer.import_pod(alice.id, pod, tickets.clone()).await?;
    dialer.import_pod(dave.id, pod, tickets).await?;
    let elsewhere = c::create_at(&dialer, &dave, [0x0e; 16]).await?;
    let (_membership, unhosted_namespace) = dialer.pod_stores(dave.id, elsewhere)?;

    let addr = bob_phone.dial_handle().addr();
    let as_bob = Contact::new(addr.clone(), identity_of(bob.id));
    let as_nobody = Contact::new(addr, identity_of(PdnId::from_bytes([0x5f; 32])));
    for (what, refused) in [
        (
            "a namespace the node does not host",
            dialer
                .sync_namespace_as_for_test(
                    dave.id,
                    unhosted_namespace,
                    as_bob.clone(),
                    identity_of(dave.id),
                )
                .await,
        ),
        (
            "an identity the node does not host",
            dialer
                .sync_namespace_as_for_test(dave.id, records, as_nobody, identity_of(dave.id))
                .await,
        ),
        (
            "a caller that is no member",
            dialer
                .sync_namespace_as_for_test(dave.id, records, as_bob.clone(), identity_of(dave.id))
                .await,
        ),
    ] {
        let refused = refused.expect_err(what);
        assert!(
            matches!(
                refused.downcast_ref::<pdn_store::net::ConnectError>(),
                Some(pdn_store::net::ConnectError::RemoteAbort(
                    pdn_store::net::AbortReason::NotFound
                ))
            ),
            "{what} was refused otherwise: {refused:#}"
        );
    }
    // Allowed: the member's own session on the same store.
    dialer
        .sync_namespace_as_for_test(alice.id, records, as_bob, identity_of(alice.id))
        .await?;

    for node in [bob_phone, dialer] {
        node.shutdown().await?;
    }
    Ok(())
}
