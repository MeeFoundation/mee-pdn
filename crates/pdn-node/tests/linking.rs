//! Device linking end to end: the linking dialogue between in-process
//! runtimes (the payload passed as a value), the full store set the reply
//! bootstraps, the non-founder chain, the refusal pairs of the
//! verify-and-burn requirement — each probed for no observable state on
//! either side — lost-reply convergence, the rollback of a link that could
//! not catch up, per-identity isolation across several linkings, and a
//! linked device serving a grant its identity established and published
//! elsewhere (connection arming by replication).

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    AcceptError, AddrInfoOptions, CatchUpTimeout, Connection, DocTicket, PrivateMetadataStore,
    ProtocolHandler, ShareMode, SyncNode,
};
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, LinkingPayload, Runtime,
    SyncService as _, UnknownIdentity, UnknownIssuer, UnsupportedLinkingVersion,
    LINKING_FORMAT_VERSION,
};
use pdn_types::{EntryPath, NodeId, PdnId};
use test_utils::{eventually, ids, wait_devices, TIMEOUT};

mod common;
use common::{
    dial_linking, dial_linking_without_reading, establish_patiently, granted_patiently,
    link_patiently, link_probe, read_frame, write_frame, LINKING_ALPN,
};

/// Wait until the probe's directory lists exactly `devices` (order-free).
async fn wait_devices_exactly(
    directory: &PrivateMetadataStore,
    devices: &[NodeId],
) -> Result<bool> {
    let mut expected: Vec<NodeId> = devices.to_vec();
    expected.sort_by_key(|d| *d.as_bytes());
    eventually(|| async {
        let mut have = directory.list_devices().await?;
        have.sort_by_key(|d| *d.as_bytes());
        Ok(have == expected)
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

/// Assert the probe's directory holds exactly `devices` and `kinds` — the
/// inviter-side state a refusal must leave untouched, checked as one act
/// because a refusal that wrote either would break the same requirement.
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

/// The positive ceremony: create on A, invite on A, link on B. The payload
/// is bearer-free with a distinct secret per invite; link's return means
/// the directory is caught up (a connection recorded before the invite
/// lists immediately, with no poll); the inviter registered the newcomer
/// itself; and the newcomer comes up with the full store set — an entry
/// written under the identity on either runtime becomes readable on the
/// other.
#[tokio::test(flavor = "multi_thread")]
async fn linking_completes_and_brings_up_the_full_store_set() -> Result<()> {
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;
    let rt_peer = Runtime::spawn().await?;
    let x = rt_a.identity().create().await?;
    let p = rt_peer.identity().create().await?;

    // Fixtures that predate the linking: a connection of X and one data
    // entry, both authored on A.
    let invite = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_peer, p, &rt_a, x, invite).await?;
    let founder_path = EntryPath::new("contact/name")?;
    rt_a.data().write(x, &founder_path, b"from-founder").await?;

    // The payload is self-contained and bearer-free: exactly the format
    // version, the inviting device's address, the one-time secret, and the
    // identity — no ticket and no identity proof (there are no such fields
    // to carry one in). Every invite's secret is distinct.
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

    // Link B (patiently — cold transport retries with fresh invites).
    link_patiently(&rt_b, &rt_a, x).await?;

    // B hosts the identity, and success implies the directory is caught
    // up: the pre-linking connection lists immediately, no poll.
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![x]);
    assert_eq!(
        rt_b.connections().list(x).await?,
        vec![p],
        "a caught-up directory must already hold the pre-linking connection record"
    );

    // The inviter registered the newcomer: B never writes its own device
    // record in this ceremony, so B's id in the device set can only be A's
    // write. Probed from a raw linked node (which registers itself too).
    let (probe_node, probe_dir) = link_probe(&rt_a, x).await?;
    assert!(
        wait_devices(&probe_dir, &[rt_a.node_id(), rt_b.node_id()]).await?,
        "the inviter-side registration did not appear in the device set"
    );

    // The full store set: B hosts the data namespace from the reply — the
    // founder's entry becomes readable on B, and an entry written on B
    // becomes readable on A.
    assert!(
        eventually(|| async {
            Ok(rt_b.data().read(x, &founder_path).await?.as_deref() == Some(&b"from-founder"[..]))
        })
        .await?,
        "the founder's entry did not reach the newcomer's data namespace"
    );
    let newcomer_path = EntryPath::new("contact/email")?;
    rt_b.data()
        .write(x, &newcomer_path, b"from-newcomer")
        .await?;
    assert!(
        eventually(|| async {
            Ok(
                rt_a.data().read(x, &newcomer_path).await?.as_deref()
                    == Some(&b"from-newcomer"[..]),
            )
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

/// The non-founder chain: device 3 links from an invite minted on device 2,
/// which was itself linked — the induction the reply's ticket minting rests
/// on: device 2 can mint a write ticket for the data namespace only because
/// its own linking reply imported one. Device sets converge to three and
/// data catches up transitively.
#[tokio::test(flavor = "multi_thread")]
async fn linking_through_a_non_founder_device() -> Result<()> {
    let rt_1 = Runtime::spawn().await?;
    let rt_2 = Runtime::spawn().await?;
    let rt_3 = Runtime::spawn().await?;
    let x = rt_1.identity().create().await?;
    let path = EntryPath::new("affiliation/group")?;
    rt_1.data().write(x, &path, b"Acme Engineering").await?;

    // Device 2 links from the founder; device 3 from device 2.
    link_patiently(&rt_2, &rt_1, x).await?;
    link_patiently(&rt_3, &rt_2, x).await?;

    // Transitive data catch-up: the founder's entry reaches device 3
    // through the namespace ticket device 2 minted.
    assert!(
        eventually(|| async {
            Ok(rt_3.data().read(x, &path).await?.as_deref() == Some(&b"Acme Engineering"[..]))
        })
        .await?,
        "data did not reach the third device through the non-founder chain"
    );

    // All three device sets converge to three — probed store-level from a
    // raw linked node (registered as a fourth device by its own probe act).
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

/// The refusal pairs of the verify-and-burn requirement, each next to its
/// allowed counterpart and each probed for no observable state on either
/// side: an expired secret; a linking invite for an unhosted identity; a
/// wrong secret (which burns nothing — the real one still links); an
/// unknown payload version (refused before dialing, typed); and a replayed
/// secret after a completed link. The already-hosted refusal has its own
/// test below.
#[tokio::test(flavor = "multi_thread")]
async fn refusals_are_uniform_and_leave_no_state() -> Result<()> {
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;
    let rt_c = Runtime::spawn().await?;
    let x = rt_a.identity().create().await?;
    let path = EntryPath::new("contact/name")?;

    // The no-state probe: X's directory watched from a raw linked node.
    // Baseline: the founder and the probe in the device set (the probe's
    // own linking registered it), and exactly the data kind from creation.
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

    // An expired secret is refused: B hosts nothing, the identity stays
    // unknown to it, and the inviter's directory is unchanged.
    let tiny = Some(Duration::from_millis(1));
    let expired = rt_a.identity().linking_invite(x, tiny).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        rt_b.identity().link(expired, TIMEOUT).await.is_err(),
        "an expired secret must be refused"
    );
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![]);
    let err = rt_b.data().read(x, &path).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());
    assert_directory_is(&probe_dir, &baseline_devices, &baseline_kinds).await?;

    // A linking invite for an unhosted identity is refused with the typed
    // error and mints nothing pending.
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

    // ...and an unknown payload version refuses before dialing, with the
    // typed error only the pre-dial check produces.
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

    // The allowed counterpart: the live secret — having survived the wrong
    // guess and the version probe — still links. Direct (not patient): this
    // must burn *this* secret so the replay below is refused; the path is
    // warm from the probe and the expired-secret dial above.
    rt_b.identity().link(live.clone(), TIMEOUT).await?;
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![x]);
    let after_link = [rt_a.node_id(), probe_id, rt_b.node_id()];
    assert!(
        wait_devices_exactly(&probe_dir, &after_link).await?,
        "the successful link must add exactly the newcomer's device record"
    );
    assert!(wait_kinds_exactly(&probe_dir, &baseline_kinds).await?);

    // A second presentation of the burned secret is refused, and both
    // sides are exactly as the first linking left them.
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

/// Linking into an already-hosted identity is refused before dialing —
/// proven by the secret surviving the refusal: the payload the hosting
/// runtime refused still links a third runtime, which could not succeed
/// had the refusal dialed and burned it.
#[tokio::test(flavor = "multi_thread")]
async fn linking_into_a_hosted_identity_refuses_before_dialing() -> Result<()> {
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;
    let rt_c = Runtime::spawn().await?;
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
/// reads the reply. The inviter holds the registration — commit precedes
/// the reply — and a fresh invite links the same device cleanly, with its
/// node id in the device set exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn a_dialogue_lost_after_commit_converges_on_a_fresh_invite() -> Result<()> {
    let rt_a = Runtime::spawn().await?;
    let x = rt_a.identity().create().await?;

    // The probe first: it watches the device set and warms the path for
    // the raw dialer below.
    let (probe_node, probe_dir) = link_probe(&rt_a, x).await?;
    assert!(
        wait_devices(&probe_dir, &[rt_a.node_id()]).await?,
        "the probe did not sync the founder's device record"
    );

    // The vanishing dialer: presents a live secret, never reads the reply.
    let vanisher = SyncNode::spawn().await?;
    let vanisher_id = vanisher.node_id();
    let payload = rt_a.identity().linking_invite(x, None).await?;
    let connection = dial_linking_without_reading(&vanisher, &payload).await?;

    // The inviter committed before replying: the registration appears in
    // the device set while the reply sits unread on the wire.
    assert!(
        wait_devices(&probe_dir, &[rt_a.node_id(), vanisher_id]).await?,
        "the registration must exist on the inviter although the reply was never read"
    );
    connection.close(0u32.into(), b"");

    // A fresh invite links the same device cleanly...
    let retry = rt_a.identity().linking_invite(x, None).await?;
    let (directory_ticket, _data_ticket) = dial_linking(&vanisher, &retry).await?;
    let vanisher_dir = PrivateMetadataStore::import(&vanisher, directory_ticket).await?;
    assert!(
        wait_devices(&vanisher_dir, &[rt_a.node_id(), vanisher_id]).await?,
        "the re-link did not bring the directory up on the once-vanished device"
    );

    // ...and the device set holds its node id once.
    let occurrences = probe_dir
        .list_devices()
        .await?
        .into_iter()
        .filter(|d| *d == vanisher_id)
        .count();
    assert_eq!(
        occurrences, 1,
        "the re-link must not duplicate the device record"
    );

    probe_node.shutdown().await?;
    vanisher.shutdown().await?;
    rt_a.shutdown().await?;
    Ok(())
}

/// A linking "inviter" that speaks the dialogue but answers every request
/// with fixed tickets to replicas whose only host is already gone, so the
/// dialing runtime's catch-up can never complete — the harness that forces
/// `link` down its rollback path.
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

/// Rollback: a link whose directory cannot catch up within the timeout
/// fails with the typed catch-up timeout, and the dialing runtime
/// afterwards hosts neither the directory nor the data namespace — the
/// identity's operations refuse as specifically unknown, not as storage
/// errors against a dropped replica.
#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_link_leaves_nothing_behind_on_the_dialing_node() -> Result<()> {
    // Mint real tickets from a scratch node, then take it away: the
    // namespaces they address end up hosted nowhere.
    let scratch = SyncNode::spawn().await?;
    let dead_directory = PrivateMetadataStore::create(&scratch).await?;
    let directory_ticket = dead_directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    scratch.create_namespace(ids::DAVE).await?;
    let data_ticket = scratch
        .share_ticket(
            ids::DAVE,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    scratch.shutdown().await?;

    // An inviter that answers the dialogue with those dead tickets.
    let fake_inviter = SyncNode::spawn_with_protocols(vec![(
        LINKING_ALPN.to_vec(),
        Box::new(DeadTicketInviter {
            directory: directory_ticket,
            data: data_ticket,
        }),
    )])
    .await?;
    let payload = LinkingPayload {
        version: LINKING_FORMAT_VERSION,
        inviter_addr: fake_inviter.dial_handle().addr(),
        secret: [0x42; 32],
        identity: ids::DAVE,
    };

    // The exchange completes, the imports land, the catch-up cannot: the
    // wait's typed timeout surfaces (retried in case the first dial fails
    // before anything is imported, which rolls back nothing).
    let rt_b = Runtime::spawn().await?;
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

    // No residue: the identity is not hosted, and its operations refuse as
    // unknown — the assertion the unregister half of the rollback exists to
    // make true (a merely dropped replica would fail as a storage error).
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![]);
    let read_err = rt_b
        .data()
        .read(ids::DAVE, &EntryPath::new("contact/name")?)
        .await
        .unwrap_err();
    assert!(
        read_err.downcast_ref::<UnknownIssuer>().is_some(),
        "reads under the rolled-back identity must refuse as unknown, got: {read_err:#}"
    );
    let write_err = rt_b
        .data()
        .write(ids::DAVE, &EntryPath::new("contact/name")?, b"residue")
        .await
        .unwrap_err();
    assert!(write_err.downcast_ref::<UnknownIssuer>().is_some());

    rt_b.shutdown().await?;
    fake_inviter.shutdown().await?;
    Ok(())
}

/// The rollback undoes what the link did, and nothing that predates it: a
/// failed link into an identity whose namespace this runtime already reached
/// through a peer's grant leaves that grant working.
///
/// The two bindings live in different maps — the pre-dial guard reads the
/// hosted set, `import_namespace` writes the node's issuer registry — so the
/// guard cannot see a granted issuer and the link proceeds. What keeps the
/// grant is that the rollback restores the binding it displaced instead of
/// forgetting the issuer outright; forgetting is permanent (`drop_doc` takes
/// the entries with it), and a rollback that destroys state it never created
/// is worse than no rollback at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_link_leaves_a_granted_namespace_of_the_same_issuer_intact() -> Result<()> {
    // Two personas of one person, as `multi_identity` models them: X on the
    // phone, Y on the laptop, connected, with X granting Y its namespace.
    let rt_phone = Runtime::spawn().await?;
    let rt_laptop = Runtime::spawn().await?;
    let x = rt_phone.identity().create().await?;
    let y = rt_laptop.identity().create().await?;

    let invite = rt_phone.connections().invite(x, None).await?;
    establish_patiently(&rt_laptop, y, &rt_phone, x, invite).await?;

    let path = EntryPath::new("shared/note")?;
    rt_phone.data().write(x, &path, b"from-x").await?;
    let grant = granted_patiently(
        &rt_phone,
        x,
        &rt_laptop,
        y,
        x,
        common::claims_on(x, &path),
        false,
    )
    .await?;

    // The whole-store grant path: binds X in the node's issuer registry,
    // and nowhere near the hosted set the link's guard consults.
    rt_laptop.data().import(x, grant.ticket).await?;
    assert!(
        eventually(|| async {
            Ok(rt_laptop.data().read(x, &path).await?.as_deref() == Some(&b"from-x"[..]))
        })
        .await?,
        "the granted namespace never synced — the premise of this test, not its subject"
    );

    // The person now adds X to the laptop too. The link fails: its tickets
    // address replicas hosted nowhere, so the catch-up cannot complete.
    let scratch = SyncNode::spawn().await?;
    let dead_directory = PrivateMetadataStore::create(&scratch).await?;
    let directory_ticket = dead_directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    scratch.create_namespace(ids::DAVE).await?;
    let data_ticket = scratch
        .share_ticket(
            ids::DAVE,
            ShareMode::Write,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    scratch.shutdown().await?;

    let fake_inviter = SyncNode::spawn_with_protocols(vec![(
        LINKING_ALPN.to_vec(),
        Box::new(DeadTicketInviter {
            directory: directory_ticket,
            data: data_ticket,
        }),
    )])
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
        rt_laptop.data().read(x, &path).await?.as_deref(),
        Some(&b"from-x"[..]),
        "the rollback destroyed a granted namespace the link never imported"
    );
    // And the link left nothing of its own: X is not hosted, so the identity's
    // own operations still refuse — the grant binding is not a hosted identity.
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

/// Linking is per identity and isolated: one runtime links into two
/// identities by two invites, and nothing of one is visible under the
/// other — including on a runtime that hosts both sources side by side.
#[tokio::test(flavor = "multi_thread")]
async fn second_identity_requires_its_own_linking() -> Result<()> {
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;
    let rt_peers = Runtime::spawn().await?;

    // A hosts two identities; each establishes its own connection before
    // any linking, so what B receives is attributable.
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

    // Paired deny: nothing of Y arrived through that act. Y is unknown to
    // B's identity-addressed services — listing, linking invites, both
    // grant operations — and to its data namespaces; specifically unknown,
    // not a generic failure.
    let err = rt_b.connections().list(y).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = rt_b.identity().linking_invite(y, None).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = rt_b
        .connections()
        .publish_grant(y, pc, y, common::nominal_claims(y), false)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = rt_b.connections().read_grants(y, pc).await.unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let err = rt_b
        .data()
        .read(y, &EntryPath::new("contact/email")?)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIssuer>().is_some());

    // Y's stores appear on B only after a separate linking act with a
    // linking invite for Y — and the two identities stay disjoint.
    link_patiently(&rt_b, &rt_a, y).await?;
    let mut hosted = rt_b.sync().hosted_identities().await?;
    hosted.sort_by_key(|identity| *identity.as_bytes());
    let mut expected: Vec<PdnId> = vec![x, y];
    expected.sort_by_key(|identity| *identity.as_bytes());
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
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;

    // Fresh runtime: no identities, a node id already.
    let node_id = rt_b.sync().node_id();
    assert_eq!(rt_b.sync().hosted_identities().await?, vec![]);

    // One created locally, one linked from A: exactly those two.
    let created = rt_b.identity().create().await?;
    let linked = rt_a.identity().create().await?;
    link_patiently(&rt_b, &rt_a, linked).await?;

    let mut hosted = rt_b.sync().hosted_identities().await?;
    hosted.sort_by_key(|identity| *identity.as_bytes());
    let mut expected = vec![created, linked];
    expected.sort_by_key(|identity| *identity.as_bytes());
    assert_eq!(hosted, expected);

    // The node id never moved.
    assert_eq!(rt_b.sync().node_id(), node_id);

    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    Ok(())
}

/// Hosting an identity arms its connections by replication, not by
/// grant-surface use. The connection and the grant are both made on the
/// phone *after* the laptop linked, so everything the laptop knows of them
/// arrived through the directory; the laptop never touches the grant
/// surface, yet serves the granted counterparty — paired, per
/// `code-practices/access-control-tests.md`, with the tightest
/// unauthorized party: a holder of the replica's ticket with no grant gets
/// nothing from the same device.
#[tokio::test(flavor = "multi_thread")]
async fn a_linked_device_serves_a_grant_established_and_published_elsewhere() -> Result<()> {
    let rt_phone = Runtime::spawn().await?;
    let rt_laptop = Runtime::spawn().await?;
    let rt_bob = Runtime::spawn().await?;
    let rt_carol = Runtime::spawn().await?;

    // The laptop links first: everything about the connection below
    // reaches it only by replication — the case where, without arming by
    // replication, a linked device would silently refuse grants its
    // identity really issued.
    let alice = rt_phone.identity().create().await?;
    link_patiently(&rt_laptop, &rt_phone, alice).await?;

    // Established, written, and granted on the phone, after the link.
    let bob = rt_bob.identity().create().await?;
    let invite = rt_phone.connections().invite(alice, None).await?;
    establish_patiently(&rt_bob, bob, &rt_phone, alice, invite).await?;
    let email = EntryPath::new("contact/email")?;
    rt_phone
        .data()
        .write(alice, &email, b"alice@example.org")
        .await?;
    granted_patiently(
        &rt_phone,
        alice,
        &rt_bob,
        bob,
        alice,
        common::claims_on(alice, &email),
        false,
    )
    .await?;

    // Positive control on the laptop's replica: device replication has
    // delivered the entry it is about to serve.
    assert!(
        eventually(|| async {
            Ok(rt_laptop.data().read(alice, &email).await?.as_deref()
                == Some(&b"alice@example.org"[..]))
        })
        .await?,
        "the entry never replicated to the laptop — the premise of this test, not its subject"
    );

    // Both recipients aim at the laptop specifically: tickets minted there
    // (read mode — addressing, not authority). Bob holds a recorded grant;
    // Carol holds only the ticket.
    let bob_ticket = rt_laptop.data().share(alice, ShareMode::Read).await?;
    rt_bob.data().import(alice, bob_ticket).await?;
    let carol_ticket = rt_laptop.data().share(alice, ShareMode::Read).await?;
    rt_carol.data().import(alice, carol_ticket).await?;

    // The laptop serves Bob under the grant published on the phone — no
    // grant-surface call ever ran on the laptop; its armer opened the pair
    // from the replicated directory. Carol's poll rides along inside the
    // same wait, so her sync attempts against the same target accumulate
    // exactly while Bob's do.
    assert!(
        eventually(|| async {
            let _nudge = rt_carol.data().list(alice, None).await?;
            Ok(rt_bob.data().read(alice, &email).await?.as_deref()
                == Some(&b"alice@example.org"[..]))
        })
        .await?,
        "the linked device never served the granted counterparty"
    );

    // Paired deny, the tightest unauthorized party: the ticket holder with
    // no grant. Bob's convergence just above is the positive control that
    // this very device is up and serving this very replica, so Carol's
    // emptiness measures classification, not liveness.
    assert!(
        rt_carol.data().list(alice, None).await?.is_empty(),
        "a bare ticket holder must get nothing from a linked device"
    );
    assert!(rt_carol.data().read(alice, &email).await?.is_none());

    rt_phone.shutdown().await?;
    rt_laptop.shutdown().await?;
    rt_bob.shutdown().await?;
    rt_carol.shutdown().await?;
    Ok(())
}
