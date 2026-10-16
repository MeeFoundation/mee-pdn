//! A member's device statements as its devices write them out of reach of
//! one another, and the device list every member device resolves from them.
//! The statements arrive by the store-level writes the pods service
//! performs, the tickets by hand.

use std::time::Duration;

use anyhow::{Context as _, Result};
use data_layer::{
    identity_of, AddrInfoOptions, AuthorId, Contact, MemberDevice, MembershipKey, PodStore,
    PodVerdicts, RecordKey, Seq, ShareMode, SpawnOptions, SyncNode, Traced, Verdict,
};
use pdn_store::SessionRole;
use pdn_types::{NodeId, PodId, RecordId};
use test_utils::{
    eventually, join_identity,
    pod::{create, device_of, host, invite, lists_device, lists_nobody, reads, tickets, Person},
    wait_devices, TIMEOUT,
};
use tokio::sync::mpsc::UnboundedReceiver;

/// Out of every scenario's reach: no pass opens a session a scenario did
/// not name.
const QUIET: Duration = Duration::from_secs(3600);

async fn node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: QUIET,
        pod_reconcile_interval: QUIET,
        ..SpawnOptions::memory()
    })
    .await
}

/// A device no node runs: a statement may list it, and nothing answers a
/// dial to it.
fn nowhere(seed: u8, author: AuthorId) -> MemberDevice {
    let key = iroh::SecretKey::from_bytes(&[seed; 32]).public();
    MemberDevice {
        node: NodeId::from_bytes(*key.as_bytes()),
        author,
    }
}

/// A dial of one of `pod`'s stores from `holder`'s replica on `from` to
/// `callee`'s on `to`, as a drawn contact is dialed.
async fn dial(
    from: &SyncNode,
    holder: &Person,
    pod: PodId,
    store: PodStore,
    to: &SyncNode,
    callee: &Person,
) -> Result<()> {
    let contact = Contact::new(to.dial_handle().addr(), identity_of(callee.id));
    from.sync_pod_with_for_test(holder.id, pod, store, contact)
        .await
}

/// `member`'s device statement at `version`, signed by `signer`'s key,
/// written on `node` under `author`.
async fn statement(
    node: &SyncNode,
    member: &Person,
    signer: &Person,
    pod: PodId,
    version: u64,
    devices: Vec<MemberDevice>,
    author: AuthorId,
) -> Result<Vec<u8>> {
    let key = MembershipKey::Devices {
        member: member.id,
        version,
    }
    .to_bytes();
    let payload = signer
        .keys
        .device_statement(&pod, version, devices)
        .encode();
    node.write_pod_entry_as_for_test(member.id, pod, PodStore::Membership, author, &key, &payload)
        .await?;
    Ok(key)
}

/// Whether `holder`'s replica on `node` comes to hold `key` in its
/// membership store with a verdict `want` accepts, read from the membership
/// view a read reports on `reports`.
async fn holds(
    node: &SyncNode,
    reports: &mut UnboundedReceiver<PodVerdicts>,
    holder: &Person,
    pod: PodId,
    key: &[u8],
    want: impl Fn(Verdict) -> bool,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        node.pod_membership_view(holder.id, pod).await?;
        let mut held = None;
        while let Ok(report) = reports.try_recv() {
            if report.identity == holder.id && report.pod == pod {
                held = Some(report.verdicts);
            }
        }
        let held = held.context("the read reported no membership view")?;
        if held
            .iter()
            .any(|(held, _author, verdict)| held.as_slice() == key && want(*verdict))
        {
            return Ok(true);
        }
        if tokio::time::Instant::now() > deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A member's laptop whose first dials reached the member's phone before
/// the phone's PMS listed it is dialed by the phone once the listing
/// arrives, and takes the pod. The phone joined on Alice's invite, so a
/// dial a gossip neighbor prompts names Alice and is refused where only Bob
/// is hosted; every pass is out of reach; and the listing is written only
/// after the laptop's record store dial — which follows its membership
/// store's — came back refused, so nothing but the listing reaches it.
#[tokio::test(flavor = "multi_thread")]
async fn a_sibling_refused_before_its_listing_arrived_is_dialed_once_it_does() -> Result<()> {
    let (alice_phone, bob_phone, bob_laptop) = (node().await?, node().await?, node().await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, bob_pms) = host(&bob_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    invite(
        &alice_phone,
        &alice,
        pod,
        &bob,
        vec![device_of(&bob_phone, &bob)?],
    )
    .await?;
    bob_phone
        .import_pod(bob.id, pod, tickets(&alice_phone, &alice, pod).await?)
        .await?;
    assert!(eventually(|| async { Ok(!lists_nobody(&bob_phone, bob.id, pod).await?) }).await?);
    let ticket = bob_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let laptop_pms = join_identity(&bob_laptop, bob.id, ticket).await?;
    assert!(wait_devices(&laptop_pms, &[bob_phone.node_id()]).await?);
    let tickets = tickets(&bob_phone, &bob, pod).await?;

    let pause = bob_laptop.pause_next_pod_start_for_test();
    let (imported, refused) = tokio::join!(bob_laptop.import_pod(bob.id, pod, tickets), async {
        pause.reached.notified().await;
        let sessions = bob_laptop
            .watch_pod_sessions(bob.id, pod, PodStore::Records)
            .await;
        pause.release.notify_one();
        let mut sessions = sessions?;
        sessions.next_with(bob_phone.node_id(), true, TIMEOUT).await
    });
    imported?;
    let refused = refused?.context("the laptop's record store never dialed the phone")?;
    assert!(
        refused.exchanged.is_err(),
        "a phone that does not list the laptop served it"
    );
    assert!(lists_nobody(&bob_laptop, bob.id, pod).await?);

    laptop_pms.add_device(bob_laptop.node_id()).await?;
    assert!(wait_devices(&bob_pms, &[bob_laptop.node_id()]).await?);
    assert!(
        eventually(|| async { Ok(!lists_nobody(&bob_laptop, bob.id, pod).await?) }).await?,
        "the phone did not dial the laptop its PMS came to list"
    );
    Ok(())
}

/// A member's device whose statement only it holds, its sibling gone,
/// reaches a member device by dialing it: that device takes the statement
/// listing it, and serves it whole from the next session on. Denied: the
/// session the member device serves the device it does not list yet admits
/// the statement alone, though the device offers an entry of its own beside
/// it, and sends nothing.
///
/// Both replicas are out of their swarm and every pass out of reach, so the
/// two dials named are the only sessions between them.
#[allow(clippy::too_many_lines)] // one scenario: the sibling gone, the first dial and the second
#[tokio::test(flavor = "multi_thread")]
async fn a_device_whose_sibling_is_gone_reaches_a_member_device_with_its_statement() -> Result<()> {
    let (alice_phone, bob_phone, bob_laptop) = (node().await?, node().await?, node().await?);
    let (alice, _) = host(&alice_phone).await?;
    let (bob, bob_pms) = host(&bob_phone).await?;
    let pod = create(&alice_phone, &alice).await?;
    let phones = device_of(&bob_phone, &bob)?;
    invite(&alice_phone, &alice, pod, &bob, vec![phones]).await?;
    bob_phone
        .import_pod(bob.id, pod, tickets(&alice_phone, &alice, pod).await?)
        .await?;
    assert!(eventually(|| async { Ok(!lists_nobody(&bob_phone, bob.id, pod).await?) }).await?);
    bob_pms.add_device(bob_laptop.node_id()).await?;
    let ticket = bob_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let laptop_pms = join_identity(&bob_laptop, bob.id, ticket).await?;
    assert!(wait_devices(&laptop_pms, &[bob_phone.node_id(), bob_laptop.node_id()]).await?);
    let alices = device_of(&alice_phone, &alice)?;
    bob_laptop
        .import_pod(bob.id, pod, tickets(&bob_phone, &bob, pod).await?)
        .await?;
    assert!(lists_device(&bob_laptop, bob.id, pod, alice.id, alices).await?);
    bob_phone.shutdown().await?;

    let membership = tickets(&alice_phone, &alice, pod)
        .await?
        .membership
        .capability
        .id();
    alice_phone
        .leave_swarm_for_test(alice.id, membership)
        .await?;
    bob_laptop.leave_swarm_for_test(bob.id, membership).await?;
    // Taken before the statement: the laptop's redials of sessions from
    // before can bring it ahead of the dial below.
    let mut traces = alice_phone.take_session_traces().context("taken once")?;
    let mut laptops_sessions = bob_laptop
        .watch_pod_sessions(bob.id, pod, PodStore::Membership)
        .await?;
    let laptops = device_of(&bob_laptop, &bob)?;
    let listing = statement(
        &bob_laptop,
        &bob,
        &bob,
        pod,
        2,
        vec![phones, laptops],
        laptops.author,
    )
    .await?;
    bob_laptop
        .write_pod_entry(bob.id, pod, PodStore::Membership, b"ext/laptop", b"x")
        .await?;
    let elsewhere = nowhere(0xa2, alices.author);
    statement(
        &alice_phone,
        &alice,
        &alice,
        pod,
        2,
        vec![alices, elsewhere],
        alices.author,
    )
    .await?;

    dial(
        &bob_laptop,
        &bob,
        pod,
        PodStore::Membership,
        &alice_phone,
        &alice,
    )
    .await?;
    assert!(
        lists_device(&alice_phone, alice.id, pod, bob.id, laptops).await?,
        "the member device did not take the unlisted device's statement"
    );
    // Denied: the first session admits the statement alone and sends nothing.
    let laptop_node = bob_laptop.node_id();
    let mut first = Vec::new();
    let mut opened = false;
    while let Ok(trace) = traces.try_recv() {
        if trace.namespace != membership {
            continue;
        }
        let laptops = trace.peer == laptop_node && trace.role == SessionRole::Accept;
        match trace.traced {
            Traced::Admitted { key, .. } if opened => first.push(key),
            // A session that admitted nothing yet is reopened: a redial from
            // before the statement, or the decision asked again.
            Traced::AllowedFiltered if laptops && first.is_empty() => opened = true,
            Traced::Denied | Traced::AllowedWhole | Traced::AllowedFiltered
                if opened && !first.is_empty() =>
            {
                break
            }
            _ => {}
        }
    }
    assert!(opened, "the member device refused the unlisted device");
    assert_eq!(first, [listing]);
    // The session that carried the statement, as the laptop saw it: later
    // ones, served whole, may already have run.
    let alice_node = alice_phone.node_id();
    let carrying = loop {
        let session = laptops_sessions
            .next_served_with(alice_node, TIMEOUT)
            .await?
            .context("no session with the member device went through")?;
        if session
            .exchanged
            .as_ref()
            .is_ok_and(|(_received, sent)| *sent > 0)
        {
            break session;
        }
    };
    assert_eq!(
        carrying.exchanged.map(|(received, _sent)| received),
        Ok(0),
        "the member device sent the device it does not list an entry"
    );

    dial(
        &bob_laptop,
        &bob,
        pod,
        PodStore::Membership,
        &alice_phone,
        &alice,
    )
    .await?;
    assert!(
        lists_device(&bob_laptop, bob.id, pod, alice.id, elsewhere).await?,
        "the member device did not serve the device it lists"
    );

    for node in [alice_phone, bob_laptop] {
        node.shutdown().await?;
    }
    Ok(())
}

/// Statements a member's devices write while out of reach of each other
/// resolve to their union on another member's device: two siblings each
/// linking a device at one version list both, a later version written from
/// a view that missed one of them leaves it listed and its claim read
/// though it never syncs again, and a lagging sibling writing its old
/// version again displaces nothing. Denied: a statement under a key that
/// is not the member's adds no device. A statement counts on its signature
/// whoever writes it, so each is written from the device whose view it
/// reflects; the device that never syncs again is an author on its
/// sibling's node, with no node of its own.
#[allow(clippy::too_many_lines)] // one scenario: the statements of two branches and their meeting
#[tokio::test(flavor = "multi_thread")]
async fn statements_written_out_of_reach_of_each_other_list_every_device() -> Result<()> {
    let (alice_phone, bob_phone, bob_laptop) = (node().await?, node().await?, node().await?);
    let mut alices = alice_phone.take_pod_verdicts().context("taken once")?;
    let (alice, _) = host(&alice_phone).await?;
    let (bob, bob_pms) = host(&bob_phone).await?;
    bob_pms.add_device(bob_laptop.node_id()).await?;
    let ticket = bob_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    let laptop_pms = join_identity(&bob_laptop, bob.id, ticket).await?;
    assert!(wait_devices(&laptop_pms, &[bob_phone.node_id(), bob_laptop.node_id()]).await?);
    let pod = create(&alice_phone, &alice).await?;
    let (b1, b2) = (device_of(&bob_phone, &bob)?, device_of(&bob_laptop, &bob)?);
    invite(&alice_phone, &alice, pod, &bob, vec![b1]).await?;
    let tickets = tickets(&alice_phone, &alice, pod).await?;
    for device in [&bob_phone, &bob_laptop] {
        device.import_pod(bob.id, pod, tickets.clone()).await?;
    }
    statement(&bob_laptop, &bob, &bob, pod, 2, vec![b1, b2], b2.author).await?;
    dial(
        &bob_laptop,
        &bob,
        pod,
        PodStore::Membership,
        &bob_phone,
        &bob,
    )
    .await?;
    dial(
        &alice_phone,
        &alice,
        pod,
        PodStore::Membership,
        &bob_phone,
        &bob,
    )
    .await?;
    assert!(lists_device(&alice_phone, alice.id, pod, bob.id, b2).await?);
    // Both of Bob's devices list Alice's own device before the stores leave
    // their swarms: one that holds her entries without their payloads yet
    // refuses her sessions, and nothing dials it again with the passes off.
    let a1 = device_of(&alice_phone, &alice)?;
    for device in [&bob_phone, &bob_laptop] {
        assert!(lists_device(device, bob.id, pod, alice.id, a1).await?);
    }
    for (device, holder) in [
        (&alice_phone, &alice),
        (&bob_phone, &bob),
        (&bob_laptop, &bob),
    ] {
        for namespace in [
            tickets.membership.capability.id(),
            tickets.records.capability.id(),
        ] {
            device.leave_swarm_for_test(holder.id, namespace).await?;
        }
    }

    // Out of reach of each other: the phone links b3 and then b5, the
    // laptop links b4, which places a claim and is never heard from again.
    let b3 = nowhere(0xb3, AuthorId::from([0xb3; 32]));
    statement(&bob_phone, &bob, &bob, pod, 3, vec![b1, b2, b3], b1.author).await?;
    let b4 = nowhere(0xb4, bob_laptop.create_author(bob.id).await?);
    statement(&bob_laptop, &bob, &bob, pod, 3, vec![b1, b2, b4], b4.author).await?;
    let claim = RecordKey::Claim {
        member: bob.id,
        id: RecordId::from_bytes([4; 16]),
        mseq: Seq::FIRST,
    };
    bob_laptop
        .write_pod_entry_as_for_test(
            bob.id,
            pod,
            PodStore::Records,
            b4.author,
            &claim.to_bytes(),
            b"from b4",
        )
        .await?;
    let b5 = nowhere(0xb5, AuthorId::from([0xb5; 32]));
    statement(
        &bob_phone,
        &bob,
        &bob,
        pod,
        4,
        vec![b1, b2, b3, b5],
        b1.author,
    )
    .await?;
    let mut alices_records = alice_phone
        .watch_pod_sessions(alice.id, pod, PodStore::Records)
        .await?;
    for device in [&bob_phone, &bob_laptop] {
        for store in [PodStore::Membership, PodStore::Records] {
            dial(&alice_phone, &alice, pod, store, device, &bob).await?;
        }
    }
    for device in [b1, b2, b3, b4, b5] {
        assert!(
            lists_device(&alice_phone, alice.id, pod, bob.id, device).await?,
            "a device a statement lists dropped out of the union"
        );
    }
    if !reads(&alice_phone, alice.id, pod, claim.record()).await? {
        let owner = alice_phone.pod_record_view(alice.id, pod).await;
        let laptop = bob_laptop.pod_record_view(bob.id, pod).await;
        let in_flight = alice_phone.pod_syncs_in_flight_for_test(pod).await;
        let mut sessions = Vec::new();
        while let Some(session) = alices_records
            .next_with(bob_laptop.node_id(), true, Duration::from_millis(50))
            .await?
        {
            sessions.push(session);
        }
        let owner_on_laptop = bob_laptop
            .pod_holding(bob.id, pod)
            .await
            .map(|holding| holding.and_then(|held| held.membership.member(&alice.id).cloned()));
        anyhow::bail!(
            "the claim of a device a later version missed did not read\nowner: {owner:#?}\n\
             laptop: {laptop:#?}\nowner's exchanges in flight: {in_flight:?}\nowner's \
             record-store sessions dialed to the laptop: {sessions:#?}\nthe owner as the \
             laptop's membership view has her: {owner_on_laptop:#?}"
        );
    }

    // The laptop, lagging at version 2 and its own version 3, writes
    // version 2 again, and the session after it brings that entry.
    statement(&bob_laptop, &bob, &bob, pod, 2, vec![b1, b2], b2.author).await?;
    let mut sessions = alice_phone
        .watch_pod_sessions(alice.id, pod, PodStore::Membership)
        .await?;
    dial(
        &alice_phone,
        &alice,
        pod,
        PodStore::Membership,
        &bob_laptop,
        &bob,
    )
    .await?;
    // A session already running as the dial is asked for can finish first,
    // with nothing new: a dial returns when asked for.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut arrived = false;
    while let Some(session) = sessions
        .next_with(
            bob_laptop.node_id(),
            true,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await?
    {
        if session.exchanged.is_ok_and(|(received, _)| received > 0) {
            arrived = true;
            break;
        }
    }
    assert!(arrived, "the old version written again did not arrive");
    // Denied: a statement for Bob under a key that is not his.
    let stranger = Person::generate();
    let b6 = nowhere(0xb6, AuthorId::from([0xb6; 32]));
    let forged = statement(
        &bob_phone,
        &bob,
        &stranger,
        pod,
        5,
        vec![b1, b2, b3, b4, b5, b6],
        b1.author,
    )
    .await?;
    dial(
        &alice_phone,
        &alice,
        pod,
        PodStore::Membership,
        &bob_phone,
        &bob,
    )
    .await?;
    assert!(
        holds(&alice_phone, &mut alices, &alice, pod, &forged, |verdict| {
            matches!(verdict, Verdict::CountedForNothing(_))
        })
        .await?
    );
    let devices = alice_phone
        .pod_membership_view(alice.id, pod)
        .await?
        .member(&bob.id)
        .map(|member| member.devices.clone())
        .unwrap_or_default();
    assert_eq!(
        devices,
        [b1, b2, b3, b4, b5].into_iter().collect(),
        "an old version or a forged statement changed the device list"
    );

    for node in [alice_phone, bob_phone, bob_laptop] {
        node.shutdown().await?;
    }
    Ok(())
}
