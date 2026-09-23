//! Device linking end to end: the linking dialogue between in-process
//! runtimes (the payload passed as a value), the full store set the reply
//! bootstraps, the non-founder chain, the refusal pairs of the
//! verify-and-burn requirement — each probed for no observable state on
//! either side — lost-reply convergence, the rollback of a link that could
//! not catch up, per-identity isolation across several linkings, and a
//! linked device serving a grant its identity established and published
//! elsewhere (connection arming by replication).

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use data_layer::{
    AcceptError, AddrInfoOptions, CatchUpTimeout, Connection, DocTicket, PrivateMetadataStore,
    ProtocolHandler, ShareMode, SyncNode,
};
use pdn_node::{
    ConnectionsService as _, DataService as _, DialogueTimeout, IdentityService as _,
    InviterUnreachable, LinkingLocalFailure, LinkingPayload, LinkingRefused, SpawnOptions,
    SyncService as _, UnknownIdentity, UnknownIssuer, UnsupportedLinkingVersion, WriteNotGranted,
    LINKING_FORMAT_VERSION,
};
use pdn_types::{EntryPath, NodeId, PdnId};
use test_utils::{eventually, ids, memory_node, wait_devices, TIMEOUT};

mod common;
use common::{
    dial_linking_without_reading_from, establish_patiently, granted_patiently, link_patiently,
    link_probe, memory_runtime, read_frame, write_frame, LINKING_ALPN,
};

/// Wait until the probe's directory lists exactly `devices` (order-free).
async fn wait_devices_exactly(
    directory: &PrivateMetadataStore,
    devices: &[NodeId],
) -> Result<bool> {
    let mut expected: Vec<NodeId> = devices.to_vec();
    expected.sort_unstable();
    eventually(|| async {
        let mut have = directory.list_devices().await?;
        have.sort_unstable();
        Ok(have == expected)
    })
    .await
}

/// Wait until the probe's directory lists every id in `devices` as pending.
async fn wait_pending_devices(
    directory: &PrivateMetadataStore,
    devices: &[NodeId],
) -> Result<bool> {
    eventually(|| async {
        let have = directory.list_pending_devices().await?;
        Ok(devices.iter().all(|d| have.contains(d)))
    })
    .await
}

/// Wait until the probe's directory lists exactly `kinds` (order-free).
async fn wait_kinds_exactly(directory: &PrivateMetadataStore, kinds: &[String]) -> Result<bool> {
    let mut expected: Vec<String> = kinds.to_vec();
    expected.sort();
    eventually(|| async {
        let mut have = directory.list_ticket_kinds().await?;
        have.sort();
        Ok(have == expected)
    })
    .await
}

/// The inviter-side state a refusal must leave untouched, checked as one
/// act.
async fn assert_directory_is(
    directory: &PrivateMetadataStore,
    devices: &[NodeId],
    kinds: &[String],
) -> Result<()> {
    assert!(
        wait_devices_exactly(directory, devices).await?,
        "the directory's device set is not what it must be"
    );
    assert!(
        wait_kinds_exactly(directory, kinds).await?,
        "the directory's ticket kinds are not what they must be"
    );
    Ok(())
}

/// The positive ceremony: create on A, invite on A, link on B. Link's
/// return means the directory is caught up (a connection recorded before
/// the invite lists with no poll), the newcomer is a confirmed device, and
/// the full store set is up — an entry written on either runtime reads on
/// the other.
#[tokio::test(flavor = "multi_thread")]
async fn linking_completes_and_brings_up_the_full_store_set() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let rt_peer = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let p = rt_peer.identity().create().await?;

    // Fixtures that predate the linking, authored on A.
    let invite = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_peer, p, &rt_a, x, invite).await?;
    let founder_path = EntryPath::new("contact/name")?;
    rt_a.data()
        .write(x, x, &founder_path, b"from-founder")
        .await?;

    // Bearer-free: format version, the inviting device's address, the
    // one-time secret, the identity — no fields for a ticket or an identity
    // proof. Every secret is distinct.
    let first = rt_a.identity().linking_invite(x, None).await?;
    let second = rt_a.identity().linking_invite(x, None).await?;
    assert_eq!(first.version, LINKING_FORMAT_VERSION);
    assert_eq!(first.identity, x);
    assert_eq!(
        NodeId::from_bytes(*first.inviter_addr.id.as_bytes()),
        rt_a.node_id(),
        "the payload must carry the inviting runtime's address"
    );
    assert_ne!(first.secret, second.secret);

    // Patiently: cold transport retries with fresh invites.
    link_patiently(&rt_b, &rt_a, x).await?;

    // Success implies the directory is caught up: no poll.
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![x]);
    assert_eq!(
        rt_b.connections().list(x).await?,
        vec![p],
        "a caught-up directory must already hold the pre-linking connection record"
    );

    // A registered it as pending, B's own confirmation made it a device;
    // probed from a raw linked node.
    let (probe_node, probe_dir) = link_probe(&rt_a, x).await?;
    assert!(
        wait_devices(&probe_dir, &[rt_a.node_id(), rt_b.node_id()]).await?,
        "the linked device did not appear in the device set"
    );

    // The full store set.
    assert!(
        eventually(|| async {
            Ok(rt_b.data().read(x, x, &founder_path).await?.as_deref()
                == Some(&b"from-founder"[..]))
        })
        .await?,
        "the founder's entry did not reach the newcomer's data namespace"
    );
    let newcomer_path = EntryPath::new("contact/email")?;
    rt_b.data()
        .write(x, x, &newcomer_path, b"from-newcomer")
        .await?;
    assert!(
        eventually(|| async {
            Ok(rt_a.data().read(x, x, &newcomer_path).await?.as_deref()
                == Some(&b"from-newcomer"[..]))
        })
        .await?,
        "the newcomer's entry did not reach the founder"
    );

    probe_node.shutdown().await?;
    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    rt_peer.shutdown().await?;
    Ok(())
}

/// Device 3 links from an invite minted on device 2, itself linked: device
/// sets converge to three and data catches up transitively. Device 2 can
/// mint a write ticket for the data namespace only because its own linking
/// reply imported one.
#[tokio::test(flavor = "multi_thread")]
async fn linking_through_a_non_founder_device() -> Result<()> {
    let rt_1 = memory_runtime().await?;
    let rt_2 = memory_runtime().await?;
    let rt_3 = memory_runtime().await?;
    let x = rt_1.identity().create().await?;
    let path = EntryPath::new("affiliation/group")?;
    rt_1.data().write(x, x, &path, b"Acme Engineering").await?;

    // Device 2 links from the founder; device 3 from device 2.
    link_patiently(&rt_2, &rt_1, x).await?;
    link_patiently(&rt_3, &rt_2, x).await?;

    // Transitive catch-up through the ticket device 2 minted.
    assert!(
        eventually(|| async {
            Ok(rt_3.data().read(x, x, &path).await?.as_deref() == Some(&b"Acme Engineering"[..]))
        })
        .await?,
        "data did not reach the third device through the non-founder chain"
    );

    // Probed from a raw linked node, itself a fourth device.
    let (probe_node, probe_dir) = link_probe(&rt_3, x).await?;
    assert!(
        wait_devices(
            &probe_dir,
            &[rt_1.node_id(), rt_2.node_id(), rt_3.node_id()],
        )
        .await?,
        "the device sets did not converge to all three devices"
    );

    probe_node.shutdown().await?;
    rt_1.shutdown().await?;
    rt_2.shutdown().await?;
    rt_3.shutdown().await?;
    Ok(())
}

/// The refusal pairs of the verify-and-burn requirement, each probed for no
/// observable state on either side: expired; an invite for an unhosted
/// identity; wrong (burns nothing); unknown payload version (refused before
/// dialing, typed); a replay after a completed link. Already-hosted has its
/// own test below.
#[tokio::test(flavor = "multi_thread")]
async fn refusals_are_uniform_and_leave_no_state() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let rt_c = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let path = EntryPath::new("contact/name")?;

    // The no-state probe: X's directory from a raw linked node; baseline is
    // the founder, the probe, and the data kind.
    let (probe_node, probe_dir) = link_probe(&rt_a, x).await?;
    let baseline_kinds = vec!["data".to_owned()];
    assert!(
        wait_kinds_exactly(&probe_dir, &baseline_kinds).await?,
        "directory probe did not sync its baseline"
    );
    let probe_id = probe_node.node_id();
    let baseline_devices = [rt_a.node_id(), probe_id];
    assert!(
        wait_devices_exactly(&probe_dir, &baseline_devices).await?,
        "directory probe did not sync the baseline device set"
    );

    // Expired: B hosts nothing, the inviter's directory is unchanged.
    let tiny = Some(Duration::from_millis(1));
    let expired = rt_a.identity().linking_invite(x, tiny).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        rt_b.identity().link(expired, TIMEOUT).await.is_err(),
        "an expired secret must be refused"
    );
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![]);
    let err = rt_b.data().read(x, x, &path).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    assert_directory_is(&probe_dir, &baseline_devices, &baseline_kinds).await?;

    // An invite for an unhosted identity mints nothing pending.
    let err = rt_a
        .identity()
        .linking_invite(ids::DAVE, None)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());

    // A wrong secret is refused and burns nothing...
    let live = rt_a.identity().linking_invite(x, None).await?;
    let forged = LinkingPayload {
        secret: [0x5a; 32],
        ..live.clone()
    };
    assert!(
        rt_c.identity().link(forged, TIMEOUT).await.is_err(),
        "a never-minted secret must be refused"
    );
    assert_eq!(rt_c.sync().hosted_identities().await?, vec![]);
    assert_directory_is(&probe_dir, &baseline_devices, &baseline_kinds).await?;

    // ...and an unknown payload version refuses before dialing, typed.
    let unversioned = LinkingPayload {
        version: 99,
        ..live.clone()
    };
    let err = rt_c
        .identity()
        .link(unversioned, TIMEOUT)
        .await
        .unwrap_err();
    let version_err = err
        .downcast_ref::<UnsupportedLinkingVersion>()
        .expect("the version refusal is typed and precedes the dial");
    assert_eq!(version_err.version, 99);

    // The live secret still links. Direct, not patient: this must burn
    // *this* secret so the replay below is refused.
    rt_b.identity().link(live.clone(), TIMEOUT).await?;
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![x]);
    let after_link = [rt_a.node_id(), probe_id, rt_b.node_id()];
    assert!(
        wait_devices_exactly(&probe_dir, &after_link).await?,
        "the successful link must add exactly the newcomer's device record"
    );
    assert!(wait_kinds_exactly(&probe_dir, &baseline_kinds).await?);

    // A replay is refused and both sides are as the first linking left them.
    assert!(
        rt_c.identity().link(live, TIMEOUT).await.is_err(),
        "a replayed secret must be refused"
    );
    assert_eq!(rt_c.sync().hosted_identities().await?, vec![]);
    assert_directory_is(&probe_dir, &after_link, &baseline_kinds).await?;

    probe_node.shutdown().await?;
    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    rt_c.shutdown().await?;
    Ok(())
}

/// A refusal that reached the inviting device downcasts to the reasonless
/// [`LinkingRefused`] — expired, wrong, replayed alike — while a dial that
/// reaches no inviting device does not. Each refusal leaves no residue. The
/// catch-up timeout's distinctness is probed in the rollback test.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_link_downcasts_where_an_unreachable_inviter_does_not() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let rt_c = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let path = EntryPath::new("contact/name")?;

    // Expired: dialed, verified, refused — the marker, and no residue.
    let tiny = Some(Duration::from_millis(1));
    let expired = rt_a.identity().linking_invite(x, tiny).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let err = rt_b.identity().link(expired, TIMEOUT).await.unwrap_err();
    assert!(
        err.downcast_ref::<LinkingRefused>().is_some(),
        "an expired-secret refusal must downcast to the marker, got: {err:#}"
    );
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![]);
    let read_err = rt_b.data().read(x, x, &path).await.unwrap_err();
    assert!(read_err.downcast_ref::<UnknownIdentity>().is_some());

    // A wrong secret: the same reasonless value.
    let live = rt_a.identity().linking_invite(x, None).await?;
    let forged = LinkingPayload {
        secret: [0x5a; 32],
        ..live.clone()
    };
    let err = rt_c.identity().link(forged, TIMEOUT).await.unwrap_err();
    assert!(
        err.downcast_ref::<LinkingRefused>().is_some(),
        "a wrong-secret refusal must downcast to the marker, got: {err:#}"
    );

    // A replayed secret, after the live one links B: the same value, and
    // no residue on the replayer.
    rt_b.identity().link(live.clone(), TIMEOUT).await?;
    let err = rt_c.identity().link(live, TIMEOUT).await.unwrap_err();
    assert!(
        err.downcast_ref::<LinkingRefused>().is_some(),
        "a replayed-secret refusal must downcast to the marker, got: {err:#}"
    );
    assert_eq!(rt_c.sync().hosted_identities().await?, vec![]);
    let read_err = rt_c.data().read(x, x, &path).await.unwrap_err();
    assert!(read_err.downcast_ref::<UnknownIdentity>().is_some());

    // A dial that reaches no inviting device: a live bare node accepting no
    // linking ALPN, rejected at once — a gone node would cost the
    // transport's whole connect timeout.
    let bystander = memory_node().await?;
    let unreachable = LinkingPayload {
        version: LINKING_FORMAT_VERSION,
        inviter_addr: bystander.dial_handle().addr(),
        secret: [0x11; 32],
        identity: ids::DAVE,
    };
    let err = rt_c
        .identity()
        .link(unreachable, TIMEOUT)
        .await
        .unwrap_err();
    // The positive half: the unreachable dial is its own typed outcome,
    // without which the negation above holds with no marker anywhere.
    assert!(
        err.downcast_ref::<InviterUnreachable>().is_some(),
        "an unreachable inviter must be recognized as its own outcome, got: {err:#}"
    );
    assert!(
        err.downcast_ref::<LinkingRefused>().is_none(),
        "a dial that reaches no inviting device must not read as a refusal, got: {err:#}"
    );

    bystander.shutdown().await?;
    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    rt_c.shutdown().await?;
    Ok(())
}

/// Linking into an already-hosted identity is refused before dialing,
/// proven by the secret surviving: the refused payload still links a third
/// runtime.
#[tokio::test(flavor = "multi_thread")]
async fn linking_into_a_hosted_identity_refuses_before_dialing() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let rt_c = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    link_patiently(&rt_b, &rt_a, x).await?;

    let fresh = rt_a.identity().linking_invite(x, None).await?;
    let err = rt_b
        .identity()
        .link(fresh.clone(), TIMEOUT)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("already hosted"),
        "linking into a hosted identity must refuse before dialing, got: {err:#}"
    );

    // The refused payload's secret was never presented: C links with it.
    rt_c.identity().link(fresh, TIMEOUT).await?;
    assert_eq!(rt_c.sync().hosted_identities().await?, vec![x]);

    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    rt_c.shutdown().await?;
    Ok(())
}

/// Lost-reply convergence: a raw dialer presents a live secret and never
/// reads the reply. The inviter's registration precedes the reply but is
/// pending, so the dialer is in no device set. A fresh invite then links
/// the same device cleanly, its node id in the set exactly once and nothing
/// left pending.
#[tokio::test(flavor = "multi_thread")]
async fn a_dialogue_lost_after_commit_converges_on_a_fresh_invite() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let x = rt_a.identity().create().await?;

    // The probe watches the device set and warms the path.
    let (probe_node, probe_dir) = link_probe(&rt_a, x).await?;
    assert!(
        wait_devices(&probe_dir, &[rt_a.node_id()]).await?,
        "the probe did not sync the founder's device record"
    );

    // The vanishing dialer: presents a live secret, never reads the reply.
    let vanisher = memory_runtime().await?;
    let vanisher_id = vanisher.node_id();
    let payload = rt_a.identity().linking_invite(x, None).await?;
    let dial = vanisher.sync().dial_handle_for_test().await;
    let connection = dial_linking_without_reading_from(&dial, &payload).await?;

    // The registration precedes the reply, as pending.
    assert!(
        wait_pending_devices(&probe_dir, &[vanisher_id]).await?,
        "the registration must exist on the inviter although the reply was never read"
    );
    // Pending is all it is: the device set holds the founder and the probe
    // alone.
    assert!(
        wait_devices_exactly(&probe_dir, &[rt_a.node_id(), probe_node.node_id()]).await?,
        "a device that never read its tickets must not be in the device set"
    );
    connection.close(0u32.into(), b"");

    // A fresh invite links the same device cleanly...
    let retry = rt_a.identity().linking_invite(x, None).await?;
    vanisher.identity().link(retry, TIMEOUT).await?;
    assert_eq!(vanisher.sync().hosted_identities().await?, vec![x]);

    // ...its node id once, the pending registration cleared.
    assert!(
        eventually(|| async {
            let occurrences = probe_dir
                .list_devices()
                .await?
                .into_iter()
                .filter(|d| *d == vanisher_id)
                .count();
            let pending = probe_dir.list_pending_devices().await?;
            Ok(occurrences == 1 && !pending.contains(&vanisher_id))
        })
        .await?,
        "the re-link must record the device once and leave nothing pending"
    );

    probe_node.shutdown().await?;
    vanisher.shutdown().await?;
    rt_a.shutdown().await?;
    Ok(())
}

/// A linking inviter that reads the request and never answers; without a
/// budget over the dialogue a dialer waits for the transport's idle
/// timeout.
#[derive(Debug)]
struct HungInviter;

impl ProtocolHandler for HungInviter {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if let Ok((_send, mut recv)) = connection.accept_bi().await {
            let _request = read_frame(&mut recv).await;
            // Never answer; hold until the dialer closes or gives up.
            connection.closed().await;
        }
        Ok(())
    }
}

/// A hung inviter costs the caller its budget and nothing more: the typed
/// dialogue timeout, asserted beside the two it must not read as — the
/// refusal and the catch-up timeout — with no residue.
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_inviter_costs_the_caller_its_budget_and_nothing_more() -> Result<()> {
    let hung = SyncNode::spawn_with(
        vec![(LINKING_ALPN.to_vec(), Box::new(HungInviter))],
        SpawnOptions::memory(),
    )
    .await?;
    let payload = LinkingPayload {
        version: LINKING_FORMAT_VERSION,
        inviter_addr: hung.dial_handle().addr(),
        secret: [0x77; 32],
        identity: ids::DAVE,
    };

    let rt = memory_runtime().await?;
    let started = std::time::Instant::now();
    let err = rt
        .identity()
        .link(payload, Duration::from_secs(1))
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        err.downcast_ref::<DialogueTimeout>().is_some(),
        "a hung dialogue must surface as the dialogue timeout, got: {err:#}"
    );
    assert!(
        err.downcast_ref::<LinkingRefused>().is_none(),
        "a hung dialogue never ended, so it must not read as a refusal"
    );
    assert!(
        err.downcast_ref::<CatchUpTimeout>().is_none(),
        "nothing was imported, so it must not read as a catch-up timeout"
    );
    // The budget was one second; a wait an order of magnitude beyond it
    // means the dialogue was not bounded by it.
    assert!(
        elapsed < Duration::from_secs(10),
        "link took {elapsed:?} against a 1s budget"
    );
    assert_eq!(rt.sync().hosted_identities().await?, vec![]);

    hung.shutdown().await?;
    rt.shutdown().await?;
    Ok(())
}

/// A linking inviter answering every request with tickets to replicas whose
/// only host is gone, so catch-up can never complete — forces `link` down
/// its rollback path.
#[derive(Debug)]
struct DeadTicketInviter {
    directory: DocTicket,
    data: DocTicket,
}

impl ProtocolHandler for DeadTicketInviter {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let served = async {
            let (mut send, mut recv) = connection.accept_bi().await.ok()?;
            // Read (and ignore) the request — every secret "verifies" here.
            read_frame(&mut recv).await.ok()?;
            let reply = postcard::to_stdvec(&(&self.directory, &self.data)).ok()?;
            write_frame(&mut send, &reply).await.ok()?;
            send.finish().ok()?;
            connection.closed().await;
            Some(())
        }
        .await;
        if served.is_none() {
            connection.close(0u32.into(), b"");
        }
        Ok(())
    }
}

/// Rollback: a link whose catch-up times out fails typed, and afterwards
/// the identity's operations refuse as specifically unknown, not as storage
/// errors against a dropped replica.
#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_link_leaves_nothing_behind_on_the_dialing_node() -> Result<()> {
    // Real tickets from a scratch node then taken away.
    let scratch = memory_node().await?;
    scratch.provision_identity(ids::DAVE).await?;
    let dead_directory = PrivateMetadataStore::create(&scratch, ids::DAVE).await?;
    let directory_ticket = dead_directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    scratch.create_namespace(ids::DAVE, ids::DAVE).await?;
    let data_ticket = scratch
        .share_ticket(
            ids::DAVE,
            ids::DAVE,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    scratch.shutdown().await?;

    // An inviter that answers the dialogue with those dead tickets.
    let fake_inviter = SyncNode::spawn_with(
        vec![(
            LINKING_ALPN.to_vec(),
            Box::new(DeadTicketInviter {
                directory: directory_ticket,
                data: data_ticket,
            }),
        )],
        SpawnOptions::memory(),
    )
    .await?;
    let payload = LinkingPayload {
        version: LINKING_FORMAT_VERSION,
        inviter_addr: fake_inviter.dial_handle().addr(),
        secret: [0x42; 32],
        identity: ids::DAVE,
    };

    // The exchange completes, the imports land, the catch-up cannot
    // (retried in case the first dial fails before anything is imported).
    let rt_b = memory_runtime().await?;
    let deadline = std::time::Instant::now() + TIMEOUT;
    let err = loop {
        let err = rt_b
            .identity()
            .link(payload.clone(), Duration::from_secs(2))
            .await
            .unwrap_err();
        if err.downcast_ref::<CatchUpTimeout>().is_some() || std::time::Instant::now() > deadline {
            break err;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert!(
        err.downcast_ref::<CatchUpTimeout>().is_some(),
        "the failure must be the catch-up timeout, got: {err:#}"
    );
    // The timeout is not the refusal.
    assert!(
        err.downcast_ref::<LinkingRefused>().is_none(),
        "a catch-up timeout must not read as a refusal"
    );

    // No residue: not hosted, refuses as unknown — the unregister half of
    // the rollback.
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![]);
    let read_err = rt_b
        .data()
        .read(ids::DAVE, ids::DAVE, &EntryPath::new("contact/name")?)
        .await
        .unwrap_err();
    assert!(
        read_err.downcast_ref::<UnknownIdentity>().is_some(),
        "reads under the rolled-back identity must refuse as unknown, got: {read_err:#}"
    );
    let write_err = rt_b
        .data()
        .write(
            ids::DAVE,
            ids::DAVE,
            &EntryPath::new("contact/name")?,
            b"residue",
        )
        .await
        .unwrap_err();
    assert!(write_err.downcast_ref::<UnknownIdentity>().is_some());

    rt_b.shutdown().await?;
    fake_inviter.shutdown().await?;
    Ok(())
}

/// A dropped `link` future leaves no residue — only `LinkRollbackGuard`'s
/// and `SelfCleaningImport`'s `Drop` catch this; the rollback test above
/// exercises a completed failure, never a future-drop.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_link_leaves_no_residue() -> Result<()> {
    let rt_inviter = memory_runtime().await?;
    let x = rt_inviter.identity().create().await?;
    let rt = Arc::new(memory_runtime().await?);
    let pause = rt.pause_next_link_after_import().await;
    let payload = rt_inviter.identity().linking_invite(x, None).await?;
    let attempt = {
        let rt = Arc::clone(&rt);
        tokio::spawn(async move { rt.identity().link(payload, TIMEOUT).await })
    };
    pause.wait_until_reached().await;
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    pause.release();
    assert!(
        eventually(|| async {
            Ok(rt.sync().hosted_identities().await?.is_empty()
                && rt.sync().tracked_doc_count(x).await? == 0
                && !rt.sync().linking_in_flight_for_test(x).await)
        })
        .await?,
        "cancelled link left hosted state, a replica, or a reservation"
    );

    rt_inviter.shutdown().await?;
    rt.shutdown().await?;
    Ok(())
}

/// A link cancelled after its import and retried on a fresh invite stays
/// hosted: the cancelled attempt's cleanup does not undo what the retry
/// did.
#[tokio::test(flavor = "multi_thread")]
async fn retry_after_cancellation_cannot_be_undone_by_old_cleanup() -> Result<()> {
    let inviter = memory_runtime().await?;
    let identity = inviter.identity().create().await?;
    let runtime = Arc::new(memory_runtime().await?);
    let pause = runtime.pause_next_link_after_import().await;
    let first_payload = inviter.identity().linking_invite(identity, None).await?;
    let first = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.identity().link(first_payload, TIMEOUT).await })
    };

    pause.wait_until_reached().await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    pause.release();

    let retry = inviter.identity().linking_invite(identity, None).await?;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if runtime
                .identity()
                .link(retry.clone(), TIMEOUT)
                .await
                .is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(runtime.sync().hosted_identities().await?, vec![identity]);

    inviter.shutdown().await?;
    runtime.shutdown().await?;
    Ok(())
}

/// An inviter whose pending-device write fails after the secret is burnt
/// refuses the dialer, reports the failure locally as
/// `LinkingLocalFailure::PendingDeviceWrite`, refuses the replay too, and
/// leaves the newcomer neither pending nor a device.
#[tokio::test(flavor = "multi_thread")]
async fn pending_write_failure_after_burn_is_locally_observable_and_grants_nothing() -> Result<()> {
    let inviter = memory_runtime().await?;
    let identity = inviter.identity().create().await?;
    let (probe, directory) = link_probe(&inviter, identity).await?;
    let dialer = memory_runtime().await?;
    let newcomer = dialer.node_id();
    let mut failures = inviter.subscribe_linking_failures().await;
    inviter.fail_next_pending_device_write_for_test().await;
    let payload = inviter.identity().linking_invite(identity, None).await?;

    let first = dialer.identity().link(payload.clone(), TIMEOUT).await;
    assert!(first
        .unwrap_err()
        .downcast_ref::<LinkingRefused>()
        .is_some());
    assert_eq!(
        failures.recv().await?,
        LinkingLocalFailure::PendingDeviceWrite { identity, newcomer }
    );
    let replay = dialer.identity().link(payload, TIMEOUT).await;
    assert!(replay
        .unwrap_err()
        .downcast_ref::<LinkingRefused>()
        .is_some());
    assert!(!directory.list_pending_devices().await?.contains(&newcomer));
    assert!(!directory.list_devices().await?.contains(&newcomer));

    dialer.shutdown().await?;
    probe.shutdown().await?;
    inviter.shutdown().await?;
    Ok(())
}

/// The rollback undoes what the link did and nothing that predates it: a
/// failed link into X leaves Y's grant on X's namespace working, replica
/// and entries intact. The two never meet — the link brings up stores for
/// X, and what Y holds under the grant sits in Y's own stores (ADR-0013)
/// — so the rollback has nothing of Y's to displace or restore.
///
/// Denied: the restored grant binding does not make X hosted here, so
/// every identity-addressed service still refuses X as unknown.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, the grant and the failed link in one place
async fn a_failed_link_leaves_a_granted_namespace_of_the_same_issuer_intact() -> Result<()> {
    // Two personas of one person: X on the phone, Y on the laptop, X
    // granting Y.
    let rt_phone = memory_runtime().await?;
    let rt_laptop = memory_runtime().await?;
    let x = rt_phone.identity().create().await?;
    let y = rt_laptop.identity().create().await?;

    let invite = rt_phone.connections().invite(x, None).await?;
    establish_patiently(&rt_laptop, y, &rt_phone, x, invite).await?;

    let path = EntryPath::new("shared/note")?;
    rt_phone.data().write(x, x, &path, b"from-x").await?;
    granted_patiently(
        &rt_phone,
        x,
        &rt_laptop,
        y,
        x,
        common::claims_on(x, &path, false),
    )
    .await?;

    // The binder binds X in the issuer registry, nowhere near the hosted set
    // the link's guard consults.
    assert!(
        eventually(|| async {
            Ok(rt_laptop.data().read(y, x, &path).await?.as_deref() == Some(&b"from-x"[..]))
        })
        .await?,
        "the granted namespace never synced — the premise of this test, not its subject"
    );

    // The link fails: its tickets address replicas hosted nowhere.
    let scratch = memory_node().await?;
    scratch.provision_identity(ids::DAVE).await?;
    let dead_directory = PrivateMetadataStore::create(&scratch, ids::DAVE).await?;
    let directory_ticket = dead_directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    scratch.create_namespace(ids::DAVE, ids::DAVE).await?;
    let data_ticket = scratch
        .share_ticket(
            ids::DAVE,
            ids::DAVE,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    scratch.shutdown().await?;

    let fake_inviter = SyncNode::spawn_with(
        vec![(
            LINKING_ALPN.to_vec(),
            Box::new(DeadTicketInviter {
                directory: directory_ticket,
                data: data_ticket,
            }),
        )],
        SpawnOptions::memory(),
    )
    .await?;
    let payload = LinkingPayload {
        version: LINKING_FORMAT_VERSION,
        inviter_addr: fake_inviter.dial_handle().addr(),
        secret: [0x42; 32],
        identity: x,
    };

    let deadline = std::time::Instant::now() + TIMEOUT;
    let err = loop {
        let err = rt_laptop
            .identity()
            .link(payload.clone(), Duration::from_secs(2))
            .await
            .unwrap_err();
        if err.downcast_ref::<CatchUpTimeout>().is_some() || std::time::Instant::now() > deadline {
            break err;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert!(
        err.downcast_ref::<CatchUpTimeout>().is_some(),
        "the failure must be the catch-up timeout, got: {err:#}"
    );

    // The grant survives the failed link, entries and all.
    assert_eq!(
        rt_laptop.data().read(y, x, &path).await?.as_deref(),
        Some(&b"from-x"[..]),
        "the rollback destroyed a granted namespace the link never imported"
    );
    // And the link left nothing of its own: a grant binding is not a hosted
    // identity.
    assert_eq!(rt_laptop.sync().hosted_identities().await?, vec![y]);
    let err = rt_laptop.connections().list(x).await.unwrap_err();
    assert!(
        err.downcast_ref::<UnknownIdentity>().is_some(),
        "a restored grant binding must not make the identity hosted, got: {err:#}"
    );

    fake_inviter.shutdown().await?;
    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    Ok(())
}

/// Linking is per identity: one runtime links into two identities by two
/// invites, and nothing of one is visible under the other.
#[tokio::test(flavor = "multi_thread")]
async fn second_identity_requires_its_own_linking() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let rt_peers = memory_runtime().await?;

    // Each establishes its own connection before any linking, so what B
    // receives is attributable.
    let x = rt_a.identity().create().await?;
    let y = rt_a.identity().create().await?;
    let pb = rt_peers.identity().create().await?;
    let pc = rt_peers.identity().create().await?;
    let invite = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_peers, pb, &rt_a, x, invite).await?;
    let invite = rt_a.connections().invite(y, None).await?;
    establish_patiently(&rt_peers, pc, &rt_a, y, invite).await?;

    // Link B into X only.
    link_patiently(&rt_b, &rt_a, x).await?;
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![x]);
    assert_eq!(
        rt_b.connections().list(x).await?,
        vec![pb],
        "X's pre-linking connection must be readable the moment link returns"
    );

    // Paired deny: Y is specifically unknown to every identity-addressed
    // service on B.
    let err = rt_b.connections().list(y).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = rt_b.identity().linking_invite(y, None).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = rt_b
        .connections()
        .publish_grant(y, pc, y, common::nominal_claims(y))
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = rt_b.connections().read_grants(y, pc).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = rt_b
        .data()
        .read(y, y, &EntryPath::new("contact/email")?)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());

    // Y arrives only by its own linking act, and the two stay disjoint.
    link_patiently(&rt_b, &rt_a, y).await?;
    let mut hosted = rt_b.sync().hosted_identities().await?;
    hosted.sort_unstable();
    let mut expected: Vec<PdnId> = vec![x, y];
    expected.sort_unstable();
    assert_eq!(hosted, expected);
    assert_eq!(rt_b.connections().list(y).await?, vec![pc]);
    assert_eq!(rt_b.connections().list(x).await?, vec![pb]);

    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    rt_peers.shutdown().await?;
    Ok(())
}

/// Hosted identities follow create and link: none on a fresh runtime,
/// exactly the created + linked ones afterwards, node id stable throughout.
#[tokio::test(flavor = "multi_thread")]
async fn hosted_identities_follow_create_and_link() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;

    // Fresh runtime: no identities, a node id already.
    let node_id = rt_b.sync().node_id();
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![]);

    // One created locally, one linked from A: exactly those two.
    let created = rt_b.identity().create().await?;
    let linked = rt_a.identity().create().await?;
    link_patiently(&rt_b, &rt_a, linked).await?;

    let mut hosted = rt_b.sync().hosted_identities().await?;
    hosted.sort_unstable();
    let mut expected = vec![created, linked];
    expected.sort_unstable();
    assert_eq!(hosted, expected);

    // The node id never moved.
    assert_eq!(rt_b.sync().node_id(), node_id);

    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    Ok(())
}

/// Hosting an identity arms its connections by replication, not by
/// grant-surface use: the connection and the grant are made on the phone
/// after the laptop linked, and the laptop serves the counterparty having
/// published nothing. The `pair_contacts` wait ahead of `read_own_grants`
/// (which opens a pair itself) is what proves the armer got there first.
/// Paired denial: Carol's laptop-minted ticket is the admitted instrument
/// (`code-practices/product-path-arrangement.md`) — the control needs
/// addressing to the serving device and no grant exists toward her. Under
/// `test-util` as a whole: without the two waits the arrangement measured
/// about 2% flaky.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario: the open pair, the record, the service, the denial
async fn a_linked_device_serves_a_grant_established_and_published_elsewhere() -> Result<()> {
    let rt_phone = memory_runtime().await?;
    let rt_laptop = memory_runtime().await?;
    let rt_bob = memory_runtime().await?;
    let rt_carol = memory_runtime().await?;
    let carol = rt_carol.identity().create().await?;

    // The laptop links first, so everything about the connection reaches it
    // by replication alone.
    let alice = rt_phone.identity().create().await?;
    link_patiently(&rt_laptop, &rt_phone, alice).await?;

    // Established, written, and granted on the phone, after the link.
    let bob = rt_bob.identity().create().await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_phone, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone
        .data()
        .write(alice, alice, &email, b"alice@example.org")
        .await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email, false),
    )
    .await?;

    // Positive control: device replication delivered the entry the laptop is
    // about to serve, and Bob's binder imported what the grant names.
    assert!(
        eventually(|| async {
            Ok(rt_laptop
                .data()
                .read(alice, alice, &email)
                .await?
                .as_deref()
                == Some(&b"alice@example.org"[..]))
        })
        .await?,
        "the entry never replicated to the laptop — the premise of this test, not its subject"
    );
    assert!(
        eventually(|| async {
            Ok(rt_bob.data().read(bob, alice, &email).await?.as_deref()
                == Some(&b"alice@example.org"[..]))
        })
        .await?,
        "the granted claim never reached Bob while the phone was up"
    );
    // The armer opened the pair, observed before anything on the grant
    // surface is called: `pair_contacts` reads the cache without opening.
    assert!(
        eventually(|| async {
            let (own, _peer) = rt_laptop.connections().pair_contacts(alice, bob).await?;
            Ok(!own.is_empty())
        })
        .await?,
        "the pair never opened on the linked device"
    );
    // And the record the laptop will serve by has reached it — the grant
    // rides best-effort replication, and killing the publisher races it.
    assert!(
        eventually(|| async {
            Ok(rt_laptop
                .connections()
                .read_own_grants(alice, bob)
                .await?
                .is_some_and(|grant| grant.issuer == alice))
        })
        .await?,
        "the grant record never reached the device that must serve by it"
    );

    // The phone goes offline; the probed update exists on the laptop alone.
    rt_phone.shutdown().await?;
    rt_laptop
        .data()
        .write(alice, alice, &email, b"alice@moved.example.org")
        .await?;
    let carol_ticket = rt_laptop
        .data()
        .share(alice, alice, ShareMode::Read)
        .await?;
    rt_carol.data().import(carol, alice, carol_ticket).await?;

    // Carol's poll rides inside the same wait, so her sync attempts against
    // the same target accumulate exactly while Bob's do.
    assert!(
        eventually(|| async {
            let _nudge = rt_carol.data().list(carol, alice, None).await?;
            Ok(rt_bob.data().read(bob, alice, &email).await?.as_deref()
                == Some(&b"alice@moved.example.org"[..]))
        })
        .await?,
        "the linked device never served the granted counterparty"
    );

    // Paired deny: Bob's convergence just above proves this device serves
    // this replica, so Carol's emptiness measures classification, not
    // liveness.
    assert!(
        rt_carol.data().list(carol, alice, None).await?.is_empty(),
        "a bare ticket identity must get nothing from a linked device"
    );
    assert!(rt_carol.data().read(carol, alice, &email).await?.is_none());

    rt_laptop.shutdown().await?;
    rt_bob.shutdown().await?;
    rt_carol.shutdown().await?;
    Ok(())
}

/// An issuer linked onto the same node as its own grant's audience keeps its
/// own access: the write is not judged by the grant it made, and withdrawing
/// that grant does not forget the issuer's own namespace — although the
/// grant-binder memo carries a record keyed by that issuer. Forces the
/// sweep via `sweep_pair_now`/`grant_bound` (`test-util`).
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario: link, own access, withdrawal, negative control
async fn a_linked_issuer_keeps_its_own_access_beside_its_grant_audience() -> Result<()> {
    let rt_x_device = memory_runtime().await?;
    let rt_shared = memory_runtime().await?;

    // Connected while apart, then one of them linked over — the only path
    // to two connected identities on one node.
    let x = rt_x_device.identity().create().await?;
    let y = rt_shared.identity().create().await?;
    let invite = rt_x_device.connections().invite(x, None).await?;
    establish_patiently(&rt_shared, y, &rt_x_device, x, invite).await?;

    let granted_path = EntryPath::new("contact/email")?;
    rt_x_device
        .data()
        .write(x, x, &granted_path, b"x@example.org")
        .await?;
    granted_patiently(
        &rt_x_device,
        x,
        &rt_shared,
        y,
        x,
        common::claims_on(x, &granted_path, false),
    )
    .await?;
    assert!(
        eventually(|| async {
            Ok(rt_shared.data().read(y, x, &granted_path).await?.as_deref()
                == Some(&b"x@example.org"[..]))
        })
        .await?,
        "the granted namespace never synced — the premise of this test, not its subject"
    );

    // X is now added to the same node Y lives on.
    link_patiently(&rt_shared, &rt_x_device, x).await?;
    let hosted = rt_shared.sync().hosted_identities().await?;
    assert!(
        hosted.len() == 2 && hosted.contains(&x) && hosted.contains(&y),
        "the shared node must host both X and Y after the link: {hosted:?}"
    );

    // Force the memo adoption so the assertions exercise the exact record
    // shape: a `(y, x, x)` binding keyed by X as issuer beside X's own
    // hosted namespace.
    rt_shared.connections().sweep_pair_now(y, x).await?;
    assert!(
        rt_shared.connections().grant_bound(y, x, x).await,
        "the sweep must have adopted the memo entry keyed by X as issuer — the premise of this test"
    );

    // Denied: Y writing outside the read-only grant is refused at the call
    // site, as it would be against a remote issuer — the courtesy judges the
    // pair of identity and issuer, never what else this node hosts.
    let refused = rt_shared
        .data()
        .write(y, x, &granted_path, b"not mine to write")
        .await
        .unwrap_err();
    assert!(
        refused.downcast_ref::<WriteNotGranted>().is_some(),
        "a co-located audience's write outside the grant must be refused up front, got: {refused:#}"
    );

    // X's write is not refused by the grant courtesy check, whose key space
    // now contains an entry naming X as issuer.
    let own_path = EntryPath::new("notes/diary")?;
    rt_shared
        .data()
        .write(x, x, &own_path, b"dear diary")
        .await?;
    assert_eq!(
        rt_shared.data().read(x, x, &own_path).await?.as_deref(),
        Some(&b"dear diary"[..]),
        "X's own write on the shared node must not be refused by its own grant to Y"
    );

    // Withdrawn on the shared node itself — withdrawing on `rt_x_device`
    // would race the replication already waited out. The record still has
    // to cross from X's own store to Y's copy of it, both held here but
    // in two identities' replicas, so the sweep is forced until it does.
    rt_shared.connections().withdraw_grant(x, y, x).await?;
    assert!(
        eventually(|| async {
            rt_shared.connections().sweep_pair_now(y, x).await?;
            Ok(!rt_shared.connections().grant_bound(y, x, x).await)
        })
        .await?,
        "the withdrawn grant must leave the binder's memo"
    );
    assert_eq!(
        rt_shared.data().read(x, x, &own_path).await?.as_deref(),
        Some(&b"dear diary"[..]),
        "revoking the grant to Y must not forget X's own namespace"
    );
    let after_withdrawal = EntryPath::new("contact/phone")?;
    rt_shared
        .data()
        .write(x, x, &after_withdrawal, b"still mine")
        .await?;
    assert_eq!(
        rt_shared
            .data()
            .read(x, x, &after_withdrawal)
            .await?
            .as_deref(),
        Some(&b"still mine"[..]),
        "X must still be able to write its own data after the grant to Y is withdrawn"
    );

    // The other half of the withdrawal: Y loses the replica it held under
    // the grant, exactly as an audience on its own device does. Co-location
    // is not what decides it.
    assert!(
        eventually(|| async {
            Ok(rt_shared
                .data()
                .read(y, x, &granted_path)
                .await
                .is_err_and(|err| err.downcast_ref::<UnknownIssuer>().is_some()))
        })
        .await?,
        "the withdrawn grant must leave the co-located audience with no replica of X"
    );

    // Paired deny: a node with no connection to X is refused as unknown.
    let rt_outsider = memory_runtime().await?;
    let outsider = rt_outsider.identity().create().await?;
    let err = rt_outsider
        .data()
        .read(outsider, x, &own_path)
        .await
        .unwrap_err();
    assert!(
        err.downcast_ref::<UnknownIssuer>().is_some(),
        "an outsider must be refused as unknown, got: {err:#}"
    );

    rt_outsider.shutdown().await?;
    rt_x_device.shutdown().await?;
    rt_shared.shutdown().await?;
    Ok(())
}
