//! Connection establishment end to end: the pairing dialogue between
//! in-process runtimes (the invite payload passed as a value), the grant
//! flow over the exchanged metadata pair, visibility from linked devices,
//! idempotent re-establishment, and the refusal pairs of the
//! verify-and-burn requirement — each refusal probed for no observable
//! state on the inviter, next to its allowed counterpart.

use std::time::Duration;

use anyhow::Result;
use data_layer::{AddrInfoOptions, ConnectionMetadataStore, PrivateMetadataStore, ShareMode};
use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, InvitePayload, Runtime,
    UnknownIdentity, UnsupportedInviteVersion, INVITE_FORMAT_VERSION,
};
use pdn_types::{EntryPath, NodeId};
use test_utils::{eventually, ids, TIMEOUT};

mod common;
use common::{establish_patiently, link_patiently, link_probe};

/// Serializes this file's tests: each spawns several runtimes (up to a
/// dozen endpoints per test), and letting four such tests bind their
/// sockets at once maximizes the cold-start burst a freshly built binary's
/// first dials already suffer (see `test_utils::TIMEOUT`). Run one at a
/// time, each with the full liveness budget to itself.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// The full flow: an invite minted on one runtime establishes from another,
/// both sides list each other, and a grant published afterwards crosses the
/// metadata pair with no new pairing — the carried ticket imports through
/// the data service and the entries sync. The invite payload is bearer-free
/// and every invite's secret is distinct and independently pending; the
/// receiving identity's directory gains exactly the pair's kinds and no
/// ticket to the peer's data namespace.
#[tokio::test(flavor = "multi_thread")]
async fn establishment_completes_and_grants_flow_end_to_end() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;
    let rt_c = Runtime::spawn().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;
    let z = rt_c.identity().create().await?;

    // The payload is self-contained and bearer-free: exactly the format
    // version, the inviter device's address, the one-time secret, and the
    // inviting identity — no ticket and no identity proof (there are no
    // such fields to carry one in).
    let first = rt_a.connections().invite(x, None).await?;
    assert_eq!(first.version, INVITE_FORMAT_VERSION);
    assert_eq!(first.inviter, x);
    assert_eq!(
        NodeId::from_bytes(*first.inviter_addr.id.as_bytes()),
        rt_a.node_id(),
        "the payload must carry the inviting runtime's address"
    );

    // Every invite carries a distinct secret, each pending independently:
    // the one minted second establishes first, and the first is still live
    // afterwards.
    let second = rt_a.connections().invite(x, None).await?;
    assert_ne!(first.secret, second.secret);
    establish_patiently(&rt_b, y, &rt_a, x, second).await?;
    establish_patiently(&rt_c, z, &rt_a, x, first).await?;

    // Both sides of each establishment list each other.
    let listed = rt_a.connections().list(x).await?;
    assert!(listed.contains(&y) && listed.contains(&z));
    assert_eq!(rt_b.connections().list(y).await?, vec![x]);
    assert_eq!(rt_c.connections().list(z).await?, vec![x]);

    // The grant flow: X writes data, grants its namespace toward Y after
    // establishment — no new pairing — and Y reads the grant, imports the
    // carried ticket, and reads the entries.
    let path = EntryPath::new("contact/name")?;
    rt_a.data().write(x, &path, b"X").await?;
    rt_a.connections().publish_grant(x, y, x).await?;
    let granted = eventually(|| async {
        Ok(rt_b
            .connections()
            .read_grants(y, x)
            .await?
            .iter()
            .any(|grant| grant.issuer == x))
    })
    .await?;
    assert!(granted, "the grant did not reach the peer over the pair");
    let grant = rt_b
        .connections()
        .read_grants(y, x)
        .await?
        .into_iter()
        .find(|grant| grant.issuer == x)
        .expect("grant just observed");
    rt_b.data().import(x, grant.ticket).await?;
    assert!(
        eventually(|| async {
            Ok(rt_b.data().read(x, &path).await?.as_deref() == Some(&b"X"[..]))
        })
        .await?,
        "granted entries did not sync to the peer"
    );

    // Paired denial, in the same place: Z is connected to X too, but on its
    // own separate pair X↔Z. The grant X published toward Y lives only in the
    // X→Y metadata store, to which Z holds no ticket — so Z reads none of it
    // (probed after Y demonstrably has it, so a leak would already have
    // surfaced), and Z never imported X's namespace, so the bytes never reach
    // it either.
    assert!(
        rt_c.connections().read_grants(z, x).await?.is_empty(),
        "the grant X published toward Y must not be visible to Z, a separate connection of X"
    );
    assert!(
        rt_c.data().read(x, &path).await.is_err(),
        "Z must not reach X's granted data — it never received the grant to import"
    );

    // The routing/grants boundary, probed at the store level: Y's directory
    // carries the metadata-pair kinds for X — and nothing else; the ticket
    // to X's data namespace lives only in the metadata store it was read
    // from.
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

/// Establishment performed on the phones is visible from the laptops: the
/// directories' connections records replicate, and each laptop opens the
/// counterpart's metadata store from its directory's tickets — grants
/// published on either phone are read on the other identity's laptop.
#[tokio::test(flavor = "multi_thread")]
async fn connection_is_visible_from_linked_devices() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let a_phone = Runtime::spawn().await?;
    let a_laptop = Runtime::spawn().await?;
    let b_phone = Runtime::spawn().await?;
    let b_laptop = Runtime::spawn().await?;

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
    // directories carry: a grant published on either phone reaches the
    // other identity's laptop.
    a_phone.connections().publish_grant(x, y, x).await?;
    b_phone.connections().publish_grant(y, x, y).await?;
    assert!(
        eventually(|| async {
            Ok(b_laptop
                .connections()
                .read_grants(y, x)
                .await?
                .iter()
                .any(|grant| grant.issuer == x))
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
                .any(|grant| grant.issuer == y))
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

/// Re-establishment converges, whichever side invites: a second
/// establishment from a fresh invite — and a third with the direction
/// swapped — leaves one connections entry per side, reuses each side's own
/// metadata replica (the directory yields the same namespace across
/// attempts), and keeps the already-published grants readable.
#[tokio::test(flavor = "multi_thread")]
async fn re_establishment_converges_and_may_swap_directions() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;

    // First establishment, plus a grant that must survive everything below.
    let invite = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_b, y, &rt_a, x, invite).await?;
    rt_a.connections().publish_grant(x, y, x).await?;
    assert!(
        eventually(|| async { Ok(!rt_b.connections().read_grants(y, x).await?.is_empty()) })
            .await?,
        "the pre-retry grant did not reach the peer"
    );

    // The directory is the identity's durable view of the pair: the own
    // store's namespace after each attempt must be the same replica.
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

    // The own store is the same replica every time: the directory still
    // yields the first namespace...
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
    rt_b.connections().publish_grant(y, x, y).await?;
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

/// The refusal pairs of the verify-and-burn requirement, each next to its
/// allowed counterpart and each probed for no observable state on the
/// inviter: an expired secret, a wrong secret (which burns nothing — the
/// real one still establishes), an unknown payload version (refused before
/// dialing), unknown-identity refusals for invite and establish, and a
/// replayed secret after a completed establishment.
#[tokio::test(flavor = "multi_thread")]
async fn refusals_are_uniform_and_leave_no_state_on_the_inviter() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;
    let rt_c = Runtime::spawn().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;
    let z = rt_c.identity().create().await?;

    // The no-state probe: X's directory, watched from a linked-device view.
    // Baseline before any establishment: exactly the data kind from
    // creation.
    let (probe_node, probe_dir) = link_probe(&rt_a, x).await?;
    let baseline = vec!["data".to_owned()];
    assert!(
        wait_kinds_exactly(&probe_dir, &baseline).await?,
        "directory probe did not sync its baseline"
    );

    // An expired secret is refused: no connections entry, no directory
    // kind, nothing listed on either side.
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

    // Unknown-identity refusals, before anything runs: an invite for an
    // identity the runtime does not host mints nothing, and establishing on
    // behalf of one refuses before dialing.
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

    // A wrong secret is refused and burns nothing: the guess fails with no
    // state anywhere...
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

    // ...and an unknown payload version refuses before dialing, with the
    // typed error only the pre-dial check produces.
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

    // The allowed counterpart: the live secret — having survived the
    // unhosted attempt, the wrong guess, and the version probe — still
    // establishes. Direct (not patient): this must burn *this* secret so the
    // replay below is refused; the path is already warm from the probe
    // import and the expired-secret dial above.
    rt_b.connections().establish(y, live.clone()).await?;
    assert_eq!(rt_a.connections().list(x).await?, vec![y]);
    let established = vec![
        "data".to_owned(),
        format!("connection-metadata/{y}/own"),
        format!("connection-metadata/{y}/peer"),
    ];
    assert!(wait_kinds_exactly(&probe_dir, &established).await?);

    // A second presentation of the burned secret is refused, and the
    // inviter's stores are exactly as the establishment left them: the one
    // connection, the same directory kinds, nothing toward the replayer.
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

/// Two runtimes invite each other and both `establish` toward the other at
/// the same time. The dialogue must not hold the runtime lock across the
/// network round-trip, or the two establishments deadlock — each holding
/// its own lock while the peer's accept side blocks on that same lock. A
/// regression would hang, so the bounded wait is the assertion.
///
/// The path is warmed first (a normal establishment, patiently) so the
/// concurrent probe below — which calls `establish` directly, no retry, to
/// exercise the exact timing — is not exposed to the freshly-built-binary
/// cold-start penalty, and reuses the already-cached replicas.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reciprocal_establishment_does_not_deadlock() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;

    // Warm the path between the endpoints and establish the pair once, so
    // the reciprocal attempts below reuse the cached replicas.
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

/// The pair follows the directory, not a stale cache. The directory is the
/// durable truth for which replicas a connection addresses; the runtime's
/// pair map is only a handle cache. Rewriting the directory's peer-kind onto
/// a different replica — what another device of the identity publishes once
/// the counterparty re-establishes onto a fresh one — must move the grant
/// reads there. Without re-validating the cache the pair would keep reading
/// the superseded replica and silently miss every later grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_follows_the_directory_not_a_stale_cache() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let rt_a = Runtime::spawn().await?;
    let rt_b = Runtime::spawn().await?;
    let x = rt_a.identity().create().await?;
    let y = rt_b.identity().create().await?;

    let invite = rt_a.connections().invite(x, None).await?;
    establish_patiently(&rt_b, y, &rt_a, x, invite).await?;

    // X grants toward Y and Y reads it: the pair is live, and now cached on B.
    let path = EntryPath::new("contact/name")?;
    rt_a.data().write(x, &path, b"X").await?;
    rt_a.connections().publish_grant(x, y, x).await?;
    assert!(
        eventually(|| async { Ok(!rt_b.connections().read_grants(y, x).await?.is_empty()) })
            .await?,
        "the grant did not reach Y"
    );

    // Stand in for another device of Y republishing the pair onto a fresh
    // replica: the linking reply carries a write ticket to Y's directory,
    // so a raw linked node does exactly what Y's own devices do.
    let (probe_node, probe_dir) = link_probe(&rt_b, y).await?;
    let replacement = ConnectionMetadataStore::create(&probe_node).await?;
    let replacement_ticket = replacement
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    probe_dir
        .put_ticket(&data_layer::peer_ticket_kind(&x), &replacement_ticket)
        .await?;

    // Once the rewrite syncs to B, its reads move to the (empty) replacement
    // replica rather than staying on the superseded cached one.
    assert!(
        eventually(|| async { Ok(rt_b.connections().read_grants(y, x).await?.is_empty()) }).await?,
        "read_grants kept reading the superseded replica from cache instead of the one the directory names"
    );

    probe_node.shutdown().await?;
    rt_a.shutdown().await?;
    rt_b.shutdown().await?;
    Ok(())
}
