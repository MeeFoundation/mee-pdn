//! Connection establishment end to end: the pairing dialogue between
//! in-process runtimes (the invite payload passed as a value), the grant
//! flow over the exchanged metadata pair, visibility from linked devices,
//! idempotent re-establishment, and the refusal pairs of the
//! verify-and-burn requirement — each refusal probed for no observable
//! state on the inviter, next to its allowed counterpart.

use std::time::Duration;

use anyhow::{Context as _, Result};
use data_layer::{
    AcceptError, AddrInfoOptions, Connection, ConnectionMetadataStore, PrivateMetadataStore,
    ProtocolHandler, ShareMode, SyncNode,
};
use pdn_node::{
    ConnectionsService as _, DataService as _, DelegationUnsupported, EstablishmentInProgress,
    EstablishmentRefused, EstablishmentTimeout, IdentityService as _, InvitePayload,
    InviterUnreachable, Runtime, SpawnOptions, UnknownIdentity, UnknownIssuer,
    UnsupportedInviteVersion, INVITE_FORMAT_VERSION,
};
use pdn_types::{EntryPath, NodeId};
use test_utils::{eventually, ids, memory_node, TIMEOUT};

mod common;
use common::{
    establish_patiently, granted_patiently, link_patiently, link_probe, memory_runtime, read_frame,
    PAIRING_ALPN,
};

/// A pairing inviter that reads the request and never answers; without the
/// dialogue ceiling a dialer against it waits for the transport's idle
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

/// A hung inviter costs the caller the dialogue ceiling and nothing more:
/// the typed establishment timeout — not the refusal, whose dialogue ended
/// — within the ceiling rather than the transport's idle timeout.
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_pairing_inviter_costs_the_ceiling_and_nothing_more() -> Result<()> {
    let hung = SyncNode::spawn_with(
        vec![(PAIRING_ALPN.to_vec(), Box::new(HungInviter))],
        SpawnOptions::memory(),
    )
    .await?;
    let rt = memory_runtime().await?;
    let y = rt.identity().create().await?;
    let invite = InvitePayload {
        version: INVITE_FORMAT_VERSION,
        inviter_addr: hung.dial_handle().addr(),
        secret: [0x55; 32],
        inviter: ids::DAVE,
    };

    let started = std::time::Instant::now();
    let err = rt.connections().establish(y, invite).await.unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        err.downcast_ref::<EstablishmentTimeout>().is_some(),
        "a hung dialogue must surface as the establishment timeout, got: {err:#}"
    );
    assert!(
        err.downcast_ref::<EstablishmentRefused>().is_none(),
        "a hung dialogue never ended, so it must not read as a refusal"
    );
    // The bound is the ceiling itself plus slack; the transport's idle
    // timeout is what a wait far beyond it would mean.
    assert!(
        elapsed < pdn_node::pairing::ESTABLISHMENT_DIALOGUE_TIMEOUT + Duration::from_secs(10),
        "establish took {elapsed:?} against the dialogue ceiling"
    );

    hung.shutdown().await?;
    rt.shutdown().await?;
    Ok(())
}

/// Shutdown waits for an accept in flight and stops waiting once it
/// returns: a raw dialer leaves the inviter's `accept` parked mid-request,
/// shutdown is still running against that, and returns once the dialer goes
/// away — well inside the handler's budget, so the accept finishing is what
/// ended the wait.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_waits_for_an_accept_in_flight_and_no_longer() -> Result<()> {
    let rt = memory_runtime().await?;
    let x = rt.identity().create().await?;
    // Minted for its address alone — the dialogue below is never completed,
    // so the secret is spent on nothing.
    let invite = rt.connections().invite(x, None).await?;

    // The dialer parks the accept: a frame header promising more bytes than
    // it sends leaves the inviter's `serve` blocked reading the rest, with
    // the handler's permit held for as long as the connection lives.
    let dialer = memory_node().await?;
    let connection = dialer
        .dial_handle()
        .connect(invite.inviter_addr.clone(), PAIRING_ALPN)
        .await?;
    let (mut send, _recv) = connection.open_bi().await?;
    send.write_all(&64u32.to_le_bytes()).await?;
    send.write_all(b"partial").await?;
    tokio::time::timeout(TIMEOUT, async {
        while !rt.pairing_accept_in_flight_for_test().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("the inviter never dispatched the parked accept")?;

    let started = std::time::Instant::now();
    let mut shutting_down = tokio::spawn(async move { rt.shutdown().await });

    tokio::task::yield_now().await;
    assert!(
        !shutting_down.is_finished(),
        "shutdown must wait for the accept in flight rather than abort it"
    );

    // The accept returns the moment its connection goes away, and shutdown
    // with it.
    connection.close(0u32.into(), b"");
    let waited = tokio::time::timeout(TIMEOUT, &mut shutting_down).await;
    let elapsed = started.elapsed();
    waited
        .context("shutdown never returned after the accept it waited for finished")?
        .context("the shutdown task panicked")??;
    assert!(
        elapsed < pdn_node::pairing::SHUTDOWN_ESTABLISHMENT_BUDGET,
        "shutdown took {elapsed:?} — its budget elapsing, not the accept finishing, ended the wait"
    );

    dialer.shutdown().await?;
    Ok(())
}

/// Denied: a second `establish` from the same identity toward the same
/// inviter while one is in flight refuses as a conflict. The hung inviter
/// parks the first attempt for its whole ceiling, so the second meets the
/// reservation with certainty, and the two secrets differ — what refuses
/// it is the pair in flight, not a burnt secret.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_establishment_toward_the_same_peer_is_refused_while_one_is_in_flight(
) -> Result<()> {
    let hung = SyncNode::spawn_with(
        vec![(PAIRING_ALPN.to_vec(), Box::new(HungInviter))],
        SpawnOptions::memory(),
    )
    .await?;
    let rt = memory_runtime().await?;
    let y = rt.identity().create().await?;
    let toward_dave = |secret| InvitePayload {
        version: INVITE_FORMAT_VERSION,
        inviter_addr: hung.dial_handle().addr(),
        secret,
        inviter: ids::DAVE,
    };

    let connections = rt.connections();
    let (first, second) = tokio::join!(
        connections.establish(y, toward_dave([0x55; 32])),
        connections.establish(y, toward_dave([0x66; 32]))
    );

    // One of the two held the reservation and paid the dialogue ceiling
    // against an inviter that never answers; the other met the reservation
    // and was refused at once. Which is which is the scheduler's business.
    let errors = [first.unwrap_err(), second.unwrap_err()];
    assert_eq!(
        errors
            .iter()
            .filter(|e| e.downcast_ref::<EstablishmentInProgress>().is_some())
            .count(),
        1,
        "exactly one of two concurrent establishments toward the same peer must refuse as a \
         conflict, got: {errors:#?}"
    );
    assert_eq!(
        errors
            .iter()
            .filter(|e| e.downcast_ref::<EstablishmentTimeout>().is_some())
            .count(),
        1,
        "the attempt that held the reservation must be the one that ran the dialogue, got: \
         {errors:#?}"
    );

    // The reservation is released on both outcomes.
    let after = rt
        .connections()
        .establish(y, toward_dave([0x77; 32]))
        .await
        .unwrap_err();
    assert!(
        after.downcast_ref::<EstablishmentInProgress>().is_none(),
        "the reservation must not outlive the attempts that took it, got: {after:#}"
    );

    hung.shutdown().await?;
    rt.shutdown().await?;
    Ok(())
}

/// A pairing inviter that reads the request and blocks forever, never
/// accepting a release — for cancelling the `establish` future itself.
#[cfg(feature = "test-util")]
#[derive(Debug)]
struct NeverAnsweringInviter;

#[cfg(feature = "test-util")]
impl ProtocolHandler for NeverAnsweringInviter {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if let Ok((_send, mut recv)) = connection.accept_bi().await {
            let _request = read_frame(&mut recv).await;
            connection.closed().await;
        }
        Ok(())
    }
}

/// A dropped `establish` future, anywhere from the dial through the round
/// trip, leaves no replica behind — only `EstablishGuard`'s `Drop` can
/// catch this; a completed-failure test never exercises it.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_establish_leaves_no_replica_behind() -> Result<()> {
    let inviter = SyncNode::spawn_with(
        vec![(PAIRING_ALPN.to_vec(), Box::new(NeverAnsweringInviter))],
        SpawnOptions::memory(),
    )
    .await?;
    let rt = memory_runtime().await?;
    let y = rt.identity().create().await?;
    let before = rt.sync().tracked_doc_count(y).await?;

    // Every delay cancels the future somewhere between the dial and the
    // reply.
    for delay in [
        Duration::from_millis(0),
        Duration::from_millis(5),
        Duration::from_millis(20),
        Duration::from_millis(80),
    ] {
        let invite = InvitePayload {
            version: INVITE_FORMAT_VERSION,
            inviter_addr: inviter.dial_handle().addr(),
            secret: [0x77; 32],
            inviter: ids::DAVE,
        };
        let connections = rt.connections();
        let attempt = connections.establish(y, invite);
        tokio::select! {
            _ = attempt => {}
            () = tokio::time::sleep(delay) => {}
        }
        assert!(
            eventually(|| async { Ok(rt.sync().tracked_doc_count(y).await? == before) }).await?,
            "cancelling establish at {delay:?} left a tracked replica behind"
        );
    }

    inviter.shutdown().await?;
    rt.shutdown().await?;
    Ok(())
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

/// The full flow: invite on one runtime, establish from another, both sides
/// list each other, and a grant published afterwards crosses the pair with
/// no new pairing. The payload is bearer-free with every secret distinct;
/// the receiving directory gains the pair's kinds and no ticket to the
/// peer's data namespace.
#[tokio::test(flavor = "multi_thread")]
async fn establishment_completes_and_grants_flow_end_to_end() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let rt_c = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;
    let z = rt_c.identity().create().await?;

    // Bearer-free: format version, the inviter device's address, the
    // one-time secret, the inviting identity — no fields for a ticket or an
    // identity proof.
    let first = rt_a.connections().invite(x, None).await?;
    assert_eq!(first.version, INVITE_FORMAT_VERSION);
    assert_eq!(first.inviter, x);
    assert_eq!(
        NodeId::from_bytes(*first.inviter_addr.id.as_bytes()),
        rt_a.node_id(),
        "the payload must carry the inviting runtime's address"
    );

    // Every secret pends independently: the second minted establishes
    // first, the first is still live.
    let second = rt_a.connections().invite(x, None).await?;
    assert_ne!(first.secret, second.secret);
    establish_patiently(&rt_b, y, &rt_a, x, second).await?;
    establish_patiently(&rt_c, z, &rt_a, x, first).await?;

    // Both sides of each establishment list each other.
    let listed = rt_a.connections().list(x).await?;
    assert!(listed.contains(&y) && listed.contains(&z));
    assert_eq!(rt_b.connections().list(y).await?, vec![x]);
    assert_eq!(rt_c.connections().list(z).await?, vec![x]);

    // The grant flow, no new pairing and no import act: Y's binder imports
    // what the grant names.
    let path = EntryPath::new("contact/name")?;
    rt_a.data().write(x, x, &path, b"X").await?;
    granted_patiently(&rt_a, x, &rt_b, y, x, common::claims_on(x, &path, true)).await?;
    assert!(
        eventually(|| async {
            Ok(rt_b.data().read(y, x, &path).await?.as_deref() == Some(&b"X"[..]))
        })
        .await?,
        "granted entries did not sync to the peer"
    );

    // The grant carries write, proven by the round trip: Y's overwrite
    // reaches X through the ingest gate (ADR-0008).
    rt_b.data().write(y, x, &path, b"Y was here").await?;
    assert!(
        eventually(|| async {
            Ok(rt_a.data().read(x, x, &path).await?.as_deref() == Some(&b"Y was here"[..]))
        })
        .await?,
        "the grantee's write did not reach the issuer — the grant's ticket is not a write ticket"
    );

    // Paired denial: Z, on its own pair with X, holds no ticket to the X→Y
    // store and never imported X's namespace — probed after Y demonstrably
    // has the grant.
    assert!(
        rt_c.connections().read_grants(z, x).await?.is_empty(),
        "the grant X published toward Y must not be visible to Z, a separate connection of X"
    );
    assert!(
        rt_c.data().read(z, x, &path).await.is_err(),
        "Z must not reach X's granted data — it never received the grant to import"
    );

    // Y's directory carries the pair's kinds for X and nothing else; the
    // data-namespace ticket lives only in the metadata store.
    let (probe_node, probe_dir) = link_probe(&rt_b, y).await?;
    assert!(
        wait_kinds_exactly(
            &probe_dir,
            &[
                "data".to_owned(),
                format!("connection-metadata/{x}/own"),
                format!("connection-metadata/{x}/peer"),
            ],
        )
        .await?,
        "the receiving directory must hold exactly the pair's kinds and no data ticket"
    );

    probe_node.shutdown().await?;
    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    rt_c.shutdown().await?;
    Ok(())
}

/// Establishment on the phones is visible from the laptops: the connections
/// records replicate, and each laptop opens the counterpart's store from
/// its directory's tickets.
#[tokio::test(flavor = "multi_thread")]
async fn connection_is_visible_from_linked_devices() -> Result<()> {
    let a_phone = memory_runtime().await?;
    let a_laptop = memory_runtime().await?;
    let b_phone = memory_runtime().await?;
    let b_laptop = memory_runtime().await?;

    // Two identities, each with a laptop linked before the pairing.
    let x = a_phone.identity().create().await?;
    let y = b_phone.identity().create().await?;
    link_patiently(&a_laptop, &a_phone, x).await?;
    link_patiently(&b_laptop, &b_phone, y).await?;

    // Pairing runs on the phones.
    let invite = a_phone.connections().invite(x, None).await?;
    establish_patiently(&b_phone, y, &a_phone, x, invite).await?;

    // Both laptops eventually list the counterparty...
    assert!(
        eventually(|| async { Ok(a_laptop.connections().list(x).await?.contains(&y)) }).await?,
        "the connection did not reach the inviter's laptop"
    );
    assert!(
        eventually(|| async { Ok(b_laptop.connections().list(y).await?.contains(&x)) }).await?,
        "the connection did not reach the scanner's laptop"
    );

    // ...and read the counterpart's metadata store from the pair their
    // directories carry.
    a_phone
        .connections()
        .publish_grant(x, y, x, common::nominal_claims(x))
        .await?;
    b_phone
        .connections()
        .publish_grant(y, x, y, common::nominal_claims(y))
        .await?;
    assert!(
        eventually(|| async {
            Ok(b_laptop
                .connections()
                .read_grants(y, x)
                .await?
                .iter()
                .any(|g| g.grant.issuer == x))
        })
        .await?,
        "X's grant did not reach Y's laptop through the directory-opened pair"
    );
    assert!(
        eventually(|| async {
            Ok(a_laptop
                .connections()
                .read_grants(x, y)
                .await?
                .iter()
                .any(|g| g.grant.issuer == y))
        })
        .await?,
        "Y's grant did not reach X's laptop through the directory-opened pair"
    );

    a_phone.shutdown().await?;
    a_laptop.shutdown().await?;
    b_phone.shutdown().await?;
    b_laptop.shutdown().await?;
    Ok(())
}

/// Re-establishment converges, whichever side invites: one connections
/// entry per side, the same own replica across attempts, earlier grants
/// still readable.
#[tokio::test(flavor = "multi_thread")]
async fn re_establishment_converges_and_may_swap_directions() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;

    // First establishment, plus a grant that must survive everything below.
    let invite = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_b, y, &rt_a, x, invite).await?;
    rt_a.connections()
        .publish_grant(x, y, x, common::nominal_claims(x))
        .await?;
    assert!(
        eventually(|| async { Ok(!rt_b.connections().read_grants(y, x).await?.is_empty()) })
            .await?,
        "the pre-retry grant did not reach the peer"
    );

    // The own store's namespace must be the same replica after each attempt.
    let (probe_node, probe_dir) = link_probe(&rt_a, x).await?;
    let own_kind = format!("connection-metadata/{y}/own");
    assert!(
        eventually(|| async { Ok(probe_dir.get_ticket(&own_kind).await?.is_some()) }).await?,
        "the own-kind ticket did not reach the directory probe"
    );
    let first_namespace = probe_dir
        .get_ticket(&own_kind)
        .await?
        .expect("just observed")
        .capability
        .id();

    // Re-establishment from a fresh invite, same direction.
    let retry = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_b, y, &rt_a, x, retry).await?;

    // The retry may swap directions: a third establishment from Y's invite.
    let swapped = rt_b.connections().invite(y, None).await?;
    establish_patiently(&rt_a, x, &rt_b, y, swapped).await?;

    // One connections entry per side, all three attempts included.
    assert_eq!(rt_a.connections().list(x).await?, vec![y]);
    assert_eq!(rt_b.connections().list(y).await?, vec![x]);

    // The same replica every time...
    assert!(
        eventually(|| async {
            Ok(probe_dir
                .get_ticket(&own_kind)
                .await?
                .is_some_and(|ticket| ticket.capability.id() == first_namespace))
        })
        .await?,
        "re-establishment must reuse the own replica, not mint a fresh one"
    );

    // ...the earlier grant is still readable over the pair, and the channel
    // stays live in both directions.
    assert!(
        !rt_b.connections().read_grants(y, x).await?.is_empty(),
        "the pre-retry grant must survive re-establishment"
    );
    rt_b.connections()
        .publish_grant(y, x, y, common::nominal_claims(y))
        .await?;
    assert!(
        eventually(|| async { Ok(!rt_a.connections().read_grants(x, y).await?.is_empty()) })
            .await?,
        "a grant published after the swapped retry did not cross"
    );

    probe_node.shutdown().await?;
    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    Ok(())
}

/// Granting another identity's data is refused as unsupported delegation:
/// the classifier scans the data issuer's connections, so an accepted
/// publish would be a silent no-op on both sides. The sharpest form: the
/// foreign issuer hosted on the same runtime. Paired: the identity's own
/// grant, published after the refusals, crosses alone.
#[tokio::test(flavor = "multi_thread")]
async fn granting_a_foreign_issuers_data_is_refused_as_unsupported_delegation() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let b = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;

    let invite = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_b, y, &rt_a, x, invite).await?;

    // Denied: a foreign issuer, even one hosted right here, refuses before
    // anything is minted or written.
    let err = rt_a
        .connections()
        .publish_grant(x, y, b, common::nominal_claims(b))
        .await
        .unwrap_err();
    assert!(
        err.downcast_ref::<DelegationUnsupported>().is_some(),
        "a foreign-issuer grant must refuse as unsupported delegation, got: {err:?}"
    );

    // Denied: the same with a one-claim set.
    let email = EntryPath::new("contact/email")?;
    let err = rt_a
        .connections()
        .publish_grant(x, y, b, common::claims_on(b, &email, false))
        .await
        .unwrap_err();
    assert!(
        err.downcast_ref::<DelegationUnsupported>().is_some(),
        "a foreign-issuer scoped grant must refuse as unsupported delegation, got: {err:?}"
    );

    // Allowed, and the proof nothing was recorded: the identity's own grant
    // is the only one the peer reads.
    rt_a.connections()
        .publish_grant(x, y, x, common::nominal_claims(x))
        .await?;
    assert!(
        eventually(|| async { Ok(!rt_b.connections().read_grants(y, x).await?.is_empty()) })
            .await?,
        "the identity's own grant did not reach the peer"
    );
    let grants = rt_b.connections().read_grants(y, x).await?;
    assert!(
        grants.iter().all(|g| g.grant.issuer == x),
        "a refused foreign-issuer grant left a record behind: {grants:?}"
    );

    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    Ok(())
}

/// The refusal pairs of the verify-and-burn requirement, each probed for no
/// observable state on the inviter: expired, wrong (burns nothing), unknown
/// payload version (refused before dialing), unknown identity for invite
/// and establish, and a replay after a completed establishment.
#[tokio::test(flavor = "multi_thread")]
async fn refusals_are_uniform_and_leave_no_state_on_the_inviter() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let rt_c = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;
    let z = rt_c.identity().create().await?;

    // The no-state probe: X's directory from a linked-device view; baseline
    // is the data kind from creation.
    let (probe_node, probe_dir) = link_probe(&rt_a, x).await?;
    let baseline = vec!["data".to_owned()];
    assert!(
        wait_kinds_exactly(&probe_dir, &baseline).await?,
        "directory probe did not sync its baseline"
    );

    // Expired: no state on either side.
    let tiny = Some(Duration::from_millis(1));
    let expired = rt_a.connections().invite(x, tiny).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        rt_b.connections().establish(y, expired).await.is_err(),
        "an expired secret must be refused"
    );
    assert!(rt_a.connections().list(x).await?.is_empty());
    assert!(rt_b.connections().list(y).await?.is_empty());
    assert!(wait_kinds_exactly(&probe_dir, &baseline).await?);

    // Unknown identity: an invite mints nothing, an establish refuses before
    // dialing.
    let err = rt_a
        .connections()
        .invite(ids::DAVE, None)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());
    let live = rt_a.connections().invite(x, None).await?;
    let err = rt_b
        .connections()
        .establish(ids::DAVE, live.clone())
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UnknownIdentity>().is_some());

    // A wrong secret burns nothing...
    let forged = InvitePayload {
        secret: [0x5a; 32],
        ..live.clone()
    };
    assert!(
        rt_c.connections().establish(z, forged).await.is_err(),
        "a never-minted secret must be refused"
    );
    assert!(rt_a.connections().list(x).await?.is_empty());
    assert!(wait_kinds_exactly(&probe_dir, &baseline).await?);

    // ...and an unknown payload version refuses before dialing, typed.
    let unversioned = InvitePayload {
        version: 99,
        ..live.clone()
    };
    let err = rt_c
        .connections()
        .establish(z, unversioned)
        .await
        .unwrap_err();
    let version_err = err
        .downcast_ref::<UnsupportedInviteVersion>()
        .expect("the version refusal is typed and precedes the dial");
    assert_eq!(version_err.version, 99);

    // The live secret still establishes. Direct, not patient: this must
    // burn *this* secret so the replay below is refused.
    rt_b.connections().establish(y, live.clone()).await?;
    assert_eq!(rt_a.connections().list(x).await?, vec![y]);
    let established = vec![
        "data".to_owned(),
        format!("connection-metadata/{y}/own"),
        format!("connection-metadata/{y}/peer"),
    ];
    assert!(wait_kinds_exactly(&probe_dir, &established).await?);

    // A replay is refused and the inviter's stores are as the establishment
    // left them.
    assert!(
        rt_c.connections().establish(z, live).await.is_err(),
        "a replayed secret must be refused"
    );
    assert_eq!(rt_a.connections().list(x).await?, vec![y]);
    assert!(rt_c.connections().list(z).await?.is_empty());
    assert!(wait_kinds_exactly(&probe_dir, &established).await?);

    probe_node.shutdown().await?;
    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    rt_c.shutdown().await?;
    Ok(())
}

/// A refusal that reached the inviter downcasts to the reasonless
/// [`EstablishmentRefused`] — expired, wrong, replayed alike — while a dial
/// that reaches no inviter does not, so a host tells refusal from
/// unreachable without matching text.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_downcasts_where_an_unreachable_inviter_does_not() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;

    // Expired: dialed, verified, refused — the marker.
    let tiny = Some(Duration::from_millis(1));
    let expired = rt_a.connections().invite(x, tiny).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let err = rt_b.connections().establish(y, expired).await.unwrap_err();
    assert!(
        err.downcast_ref::<EstablishmentRefused>().is_some(),
        "an expired-secret refusal must downcast to the marker, got: {err:#}"
    );

    // A wrong secret: the same reasonless value.
    let live = rt_a.connections().invite(x, None).await?;
    let forged = InvitePayload {
        secret: [0x5a; 32],
        ..live.clone()
    };
    let err = rt_b.connections().establish(y, forged).await.unwrap_err();
    assert!(
        err.downcast_ref::<EstablishmentRefused>().is_some(),
        "a wrong-secret refusal must downcast to the marker, got: {err:#}"
    );

    // A replayed secret, after the live one establishes: the same value.
    rt_b.connections().establish(y, live.clone()).await?;
    let err = rt_b.connections().establish(y, live).await.unwrap_err();
    assert!(
        err.downcast_ref::<EstablishmentRefused>().is_some(),
        "a replayed-secret refusal must downcast to the marker, got: {err:#}"
    );

    // A dial that reaches no inviter: a live bare node accepting no pairing
    // ALPN, rejected at once — a gone node would cost the transport's whole
    // connect timeout.
    let bystander = memory_node().await?;
    let unreachable = InvitePayload {
        version: INVITE_FORMAT_VERSION,
        inviter_addr: bystander.dial_handle().addr(),
        secret: [0x11; 32],
        inviter: x,
    };
    let err = rt_b
        .connections()
        .establish(y, unreachable)
        .await
        .unwrap_err();
    // The positive half: the unreachable dial is its own typed outcome,
    // without which the negation above holds with no marker anywhere.
    assert!(
        err.downcast_ref::<InviterUnreachable>().is_some(),
        "an unreachable inviter must be recognized as its own outcome, got: {err:#}"
    );
    assert!(
        err.downcast_ref::<EstablishmentRefused>().is_none(),
        "a dial that reaches no inviter must not read as a refusal, got: {err:#}"
    );

    bystander.shutdown().await?;
    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    Ok(())
}

/// Both runtimes `establish` toward each other at once. The dialogue must
/// not hold the runtime lock across the round-trip, or the two deadlock;
/// the bounded wait is the assertion. The path is warmed first so the
/// direct, no-retry probe pays no first-use setup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reciprocal_establishment_does_not_deadlock() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;

    // Warm the path and establish once.
    let warm = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_b, y, &rt_a, x, warm).await?;

    // The probe: a fresh invite each way, both scanned concurrently.
    let inv_a = rt_a.connections().invite(x, None).await?;
    let inv_b = rt_b.connections().invite(y, None).await?;
    let ca = rt_a.connections();
    let cb = rt_b.connections();
    let (ra, rb) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(ca.establish(x, inv_b), cb.establish(y, inv_a))
    })
    .await
    .expect("reciprocal establishment deadlocked");
    ra?;
    rb?;

    // Still one connection entry per side, both directions live.
    assert_eq!(rt_a.connections().list(x).await?, vec![y]);
    assert_eq!(rt_b.connections().list(y).await?, vec![x]);

    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    Ok(())
}

/// The pair follows the directory, not the pair-map cache: a peer-kind
/// rewritten onto a fresh replica — what another device publishes once the
/// counterparty re-establishes — moves grant reads there. Without
/// re-validating the cache the pair would silently miss every later grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_follows_the_directory_not_a_stale_cache() -> Result<()> {
    let rt_a = memory_runtime().await?;
    let rt_b = memory_runtime().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;

    let invite = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_b, y, &rt_a, x, invite).await?;

    // X grants toward Y and Y reads it: the pair is live, and now cached on B.
    let path = EntryPath::new("contact/name")?;
    rt_a.data().write(x, x, &path, b"X").await?;
    rt_a.connections()
        .publish_grant(x, y, x, common::nominal_claims(x))
        .await?;
    assert!(
        eventually(|| async { Ok(!rt_b.connections().read_grants(y, x).await?.is_empty()) })
            .await?,
        "the grant did not reach Y"
    );

    // Stand in for another device of Y: the linking reply carries a write
    // ticket to Y's directory.
    let (probe_node, probe_dir) = link_probe(&rt_b, y).await?;
    let replacement = ConnectionMetadataStore::create(&probe_node, y).await?;
    let replacement_ticket = replacement
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    probe_dir
        .put_ticket(&data_layer::peer_ticket_kind(&x), &replacement_ticket)
        .await?;

    // Reads move to the (empty) replacement replica.
    assert!(
        eventually(|| async { Ok(rt_b.connections().read_grants(y, x).await?.is_empty()) }).await?,
        "read_grants kept reading the superseded replica from cache instead of the one the directory names"
    );

    probe_node.shutdown().await?;
    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    Ok(())
}

/// A pair open that fails part-way leaves no replica open: the armer
/// retries every sweep, and each attempt imports both halves before it
/// arms. Arming is failed from before the link, so every sweep imports
/// again; the failure count is the positive control.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_pair_open_leaves_no_replica_behind() -> Result<()> {
    let phone = memory_runtime().await?;
    let peer = memory_runtime().await?;
    let alice = phone.identity().create().await?;
    let bob = peer.identity().create().await?;
    let invite = phone.connections().invite(alice, None).await?;
    establish_patiently(&peer, bob, &phone, alice, invite).await?;

    // A sweep cadence the poll budget can wait on.
    let laptop = Runtime::spawn(SpawnOptions {
        reconcile_interval: Duration::from_millis(200),
        ..SpawnOptions::memory()
    })
    .await?;
    // Armed before the laptop knows of any connection, so no sweep of its
    // armer can slip past and cache the pair.
    laptop.fail_pair_arm_for_test().await;
    link_patiently(&laptop, &phone, alice).await?;

    let settled = laptop.sync().tracked_doc_count(alice).await?;
    assert!(
        eventually(|| async { Ok(laptop.pair_arm_failures_for_test().await >= 4) }).await?,
        "the linked device never attempted to open the pair, so nothing here is a denial"
    );
    assert!(
        eventually(|| async { Ok(laptop.sync().tracked_doc_count(alice).await? <= settled) })
            .await?,
        "repeated failed pair opens left replicas open: {} tracked against {settled} before them",
        laptop.sync().tracked_doc_count(alice).await?
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    peer.shutdown().await?;
    Ok(())
}

/// Two identities of one node establish a connection inside the process:
/// iroh refuses a connection to this node's own endpoint id, so the same
/// dialogue runs over a pipe (ADR-0013). Both list the connection, and a
/// grant from one reaches the other with no peer reachable at all.
///
/// Denied: the invite's secret is burned by the establishment, so a
/// replay of it is refused; and a third identity hosted beside them
/// lists neither of them and reaches nothing of what they published.
#[tokio::test(flavor = "multi_thread")]
async fn two_identities_of_one_node_establish_inside_the_process() -> Result<()> {
    // The grant binder acts on its connection armer's sweep, whose cadence
    // is the reconcile interval; the default one would make this scenario
    // wait tens of seconds for an act that takes microseconds.
    let rt = Runtime::spawn(SpawnOptions {
        reconcile_interval: Duration::from_millis(500),
        ..SpawnOptions::memory()
    })
    .await?;
    let work = rt.identity().create().await?;
    let leisure = rt.identity().create().await?;
    let outsider = rt.identity().create().await?;

    let invite = rt.connections().invite(work, None).await?;
    rt.connections().establish(leisure, invite.clone()).await?;

    // Both sides assembled the same connection, mirrored.
    assert_eq!(rt.connections().list(work).await?, vec![leisure]);
    assert_eq!(rt.connections().list(leisure).await?, vec![work]);

    // A grant crosses the pair and the granted claim follows it, with no
    // node but this one running.
    let path = EntryPath::new("contact/email")?;
    rt.data()
        .write(work, work, &path, b"alice@work.example")
        .await?;
    granted_patiently(
        &rt,
        work,
        &rt,
        leisure,
        work,
        common::claims_on(work, &path, false),
    )
    .await?;
    assert!(
        eventually(|| async {
            Ok(rt.data().read(leisure, work, &path).await?.as_deref()
                == Some(&b"alice@work.example"[..]))
        })
        .await?,
        "the granted claim did not cross between two identities of one node"
    );

    // Denied: the secret was burned by the establishment above.
    let replayed = rt.connections().establish(outsider, invite).await;
    assert!(
        replayed.is_err(),
        "a replayed invite must be refused, even from a co-located identity"
    );

    // Denied: a third identity of this node sees neither the connection
    // nor what the two published.
    assert!(
        rt.connections().list(outsider).await?.is_empty(),
        "a co-located identity must not list a connection it is not party to"
    );
    let unknown = rt
        .data()
        .read(outsider, work, &path)
        .await
        .expect_err("a co-located identity must not reach an issuer it holds nothing of");
    assert!(
        unknown.downcast_ref::<UnknownIssuer>().is_some(),
        "the refusal did not read as an unknown issuer: {unknown:#}"
    );

    rt.shutdown().await?;
    Ok(())
}
