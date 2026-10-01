//! Sessions on a cell's stores, served by the member the caller
//! names: a sibling device by its identity's own directory, another member's
//! device by that member's device statements, and every other caller refused
//! as for an unhosted replica. The entries a cell's creation and its joins
//! write arrive by the store-level writes the cells service performs, and
//! the tickets by hand. The reconcile pass is set out of reach and the
//! sessions a scenario asserts on are opened by name; the ones the swarm
//! opens between member devices carry nothing a refusal below depends on.

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    identity_of, AddrInfoOptions, AnnouncementKeyPair, CellStore, CellTickets, Contact, EventKind,
    MemberDevice, MemberState, MembershipKey, NamespaceId, PrivateMetadataStore, Seq, SpawnOptions,
    SyncNode, Verdict,
};
use pdn_types::{CellId, PdnId};
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

/// An identity whose `PdnId` derives from the announcement key pair beside it.
struct Identity {
    keys: AnnouncementKeyPair,
    id: PdnId,
}

async fn host(node: &SyncNode) -> Result<(Identity, PrivateMetadataStore)> {
    let keys = AnnouncementKeyPair::generate();
    let id = keys.pdn_id();
    let directory = host_identity(node, id).await?;
    Ok((Identity { keys, id }, directory))
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
    cell: CellId,
    key: MembershipKey,
    payload: Vec<u8>,
) -> Result<()> {
    node.write_cell_entry(
        writer.id,
        cell,
        CellStore::Membership,
        &key.to_bytes(),
        &payload,
    )
    .await
}

async fn statement(
    node: &SyncNode,
    writer: &Identity,
    cell: CellId,
    member: &Identity,
    version: u64,
    devices: Vec<MemberDevice>,
) -> Result<()> {
    let key = MembershipKey::Devices {
        member: member.id,
        version,
    };
    let payload = member.keys.device_statement(version, devices).encode();
    write(node, writer, cell, key, payload).await
}

/// `creator`'s cell on `node`: both stores, the founding event, the
/// creator's first device statement, and the tickets to both stores.
async fn found(node: &SyncNode, creator: &Identity) -> Result<(CellId, CellTickets)> {
    let founding = creator.keys.founding([0x5a; 16]);
    let cell = data_layer::cell_id_of(&creator.id, &founding.announcement_key, &founding.nonce);
    node.create_cell(creator.id, cell).await?;
    write(
        node,
        creator,
        cell,
        MembershipKey::founded(creator.id),
        founding.encode(),
    )
    .await?;
    statement(
        node,
        creator,
        cell,
        creator,
        1,
        vec![device_of(node, creator)?],
    )
    .await?;
    let tickets = node
        .share_cell_tickets(creator.id, cell, AddrInfoOptions::Addresses)
        .await?;
    Ok((cell, tickets))
}

/// The invite act for `newcomer` at its sequence 1, naming `inviter`'s
/// `inviter_seq`, and the newcomer's first device statement, both written on
/// the inviter's node as the join dialogue writes them.
async fn invite(
    node: &SyncNode,
    inviter: &Identity,
    inviter_seq: u64,
    cell: CellId,
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
    let join_statement = newcomer.keys.join_statement(&cell, Seq::new(1));
    write(node, inviter, cell, key, join_statement.encode()).await?;
    statement(node, inviter, cell, newcomer, 1, vec![newcomer_device]).await
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

async fn state_on(node: &SyncNode, holder: PdnId, cell: CellId, member: PdnId) -> MemberState {
    match node.cell_membership(holder, cell).await {
        Ok(membership) => membership
            .member(&member)
            .map(|member| member.state)
            .unwrap_or_default(),
        Err(_unknown) => MemberState::default(),
    }
}

/// A member's device is served both stores and folds the cell from nothing.
/// Denied: a holder of both tickets that is no member, on either store, and
/// the member itself once kicked, on the record store.
#[allow(clippy::too_many_lines)] // one scenario: the served member beside each denial
#[tokio::test(flavor = "multi_thread")]
async fn a_member_device_is_served_and_a_ticket_holder_is_not() -> Result<()> {
    let (alice_phone, bob_phone, dave_phone) =
        (node(QUIET).await?, node(QUIET).await?, node(QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (dave, _) = host(&dave_phone).await?;
    let (cell, tickets) = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        1,
        cell,
        &bob,
        device_of(&bob_phone, &bob)?,
    )
    .await?;
    let (membership, records) = (
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    );
    let mut verdicts = alice_phone
        .take_cell_verdicts()
        .expect("the verdict channel is taken once");
    bob_phone.import_cell(bob.id, cell, tickets.clone()).await?;
    dave_phone.import_cell(dave.id, cell, tickets).await?;

    // A plain member's promotion of itself, carried to Alice's phone by the
    // session that follows.
    let own_promotion = MembershipKey::Event {
        subject: bob.id,
        seq: Seq::new(2),
        kind: EventKind::Promoted,
        actor: bob.id,
        actor_seq: Seq::new(1),
    };
    write(&bob_phone, &bob, cell, own_promotion, vec![0]).await?;
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
        eventually(|| async { Ok(state_on(&bob_phone, bob.id, cell, alice.id).await == OWNER) })
            .await?,
        "the member's device did not fold the cell from its first session"
    );
    assert_eq!(state_on(&bob_phone, bob.id, cell, bob.id).await, PLAIN);
    // A dialer serves a callee only once it resolves the callee's member,
    // so the promotion leaves Bob's phone in the second session.
    session(
        &bob_phone,
        bob.id,
        membership,
        &alice_phone,
        alice.id,
        bob.id,
    )
    .await?;
    let promotion_key = own_promotion.to_bytes();
    let judged = tokio::time::timeout(TIMEOUT, async {
        loop {
            alice_phone.cell_membership(alice.id, cell).await?;
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
    assert_eq!(state_on(&alice_phone, alice.id, cell, bob.id).await, PLAIN);

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
            .cell_membership(dave.id, cell)
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
    // Denied: the member once kicked, on the record store; the membership
    // store is served to it over the kick's past.
    let kick = MembershipKey::Event {
        subject: bob.id,
        seq: Seq::new(3),
        kind: EventKind::Kicked,
        actor: alice.id,
        actor_seq: Seq::new(1),
    };
    write(&alice_phone, &alice, cell, kick, vec![0]).await?;
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
    let (cell, tickets) = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        1,
        cell,
        &bob,
        device_of(&tablet, &bob)?,
    )
    .await?;
    let membership = tickets.membership.capability.id();
    tablet.import_cell(bob.id, cell, tickets).await?;

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
/// identity's directory, and by another member once the statement it wrote
/// reaches that member. Paired denials: the same device refused by the other
/// member before that statement exists, and by the sibling a device naming
/// the identity that the identity's directory does not list.
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
    let (bob, bob_directory) = host(&bob_phone).await?;
    let (dave, _) = host(&dave_phone).await?;
    let (cell, tickets) = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        1,
        cell,
        &bob,
        device_of(&bob_phone, &bob)?,
    )
    .await?;
    let membership = tickets.membership.capability.id();
    bob_phone.import_cell(bob.id, cell, tickets.clone()).await?;
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
        eventually(|| async { Ok(state_on(&bob_phone, bob.id, cell, alice.id).await == OWNER) })
            .await?
    );

    bob_directory.add_device(bob_laptop.node_id()).await?;
    let ticket = bob_directory
        .share_ticket(data_layer::ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let laptop_directory = join_identity(&bob_laptop, bob.id, ticket).await?;
    assert!(
        wait_devices(
            &laptop_directory,
            &[bob_phone.node_id(), bob_laptop.node_id()]
        )
        .await?,
        "the laptop's directory did not list both devices"
    );
    bob_laptop
        .import_cell(bob.id, cell, tickets.clone())
        .await?;
    dave_phone.import_cell(dave.id, cell, tickets).await?;

    // Denied: the laptop, before any statement lists it.
    assert!(refused(
        session(
            &bob_laptop,
            bob.id,
            membership,
            &alice_phone,
            alice.id,
            bob.id
        )
        .await
    ));
    session(&bob_laptop, bob.id, membership, &bob_phone, bob.id, bob.id).await?;
    // Denied: a device naming Bob that Bob's directory does not list.
    assert!(refused(
        session(&dave_phone, dave.id, membership, &bob_phone, bob.id, bob.id).await
    ));

    // The laptop's own statement: the phone and itself, under Bob's key.
    let devices = vec![device_of(&bob_phone, &bob)?, device_of(&bob_laptop, &bob)?];
    statement(&bob_laptop, &bob, cell, &bob, 2, devices).await?;
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
            Ok(session(
                &bob_laptop,
                bob.id,
                membership,
                &alice_phone,
                alice.id,
                bob.id,
            )
            .await
            .is_ok())
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
/// reached, and served once the event arrives there.
#[allow(clippy::too_many_lines)] // one scenario: the refusal and the arrival that ends it
#[tokio::test(flavor = "multi_thread")]
async fn a_newcomer_is_served_once_its_joined_event_arrives() -> Result<()> {
    let (alice_phone, bob_phone, carol_phone) =
        (node(QUIET).await?, node(QUIET).await?, node(QUIET).await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, _) = host(&bob_phone).await?;
    let (carol, _) = host(&carol_phone).await?;
    let (cell, tickets) = found(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        1,
        cell,
        &bob,
        device_of(&bob_phone, &bob)?,
    )
    .await?;
    let membership = tickets.membership.capability.id();
    bob_phone.import_cell(bob.id, cell, tickets.clone()).await?;
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
        eventually(|| async { Ok(state_on(&bob_phone, bob.id, cell, bob.id).await == PLAIN) })
            .await?
    );
    // Out of the swarm, so Carol's joined event reaches Alice's phone only
    // by the session named below.
    bob_phone.leave_swarm_for_test(bob.id, membership).await?;
    invite(
        &bob_phone,
        &bob,
        1,
        cell,
        &carol,
        device_of(&carol_phone, &carol)?,
    )
    .await?;
    carol_phone.import_cell(carol.id, cell, tickets).await?;

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
        eventually(|| async {
            Ok(state_on(&carol_phone, carol.id, cell, carol.id).await == PLAIN)
        })
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
    let (cell, tickets) = found(&tablet, &bob).await?;
    invite(&tablet, &bob, 1, cell, &dave, device_of(&tablet, &dave)?).await?;
    tablet.import_cell(dave.id, cell, tickets.clone()).await?;
    tablet.import_cell(erin.id, cell, tickets).await?;

    assert!(
        eventually(|| async {
            Ok(state_on(&tablet, dave.id, cell, bob.id).await == OWNER
                && state_on(&tablet, dave.id, cell, dave.id).await == PLAIN)
        })
        .await?,
        "the co-located member did not converge"
    );
    // Sentinel: the passes that brought Dave the store reached Erin's pairs too.
    assert_eq!(
        tablet
            .cell_membership(erin.id, cell)
            .await?
            .identities()
            .count(),
        0,
        "the co-located ticket holder took entries"
    );

    tablet.shutdown().await?;
    Ok(())
}
