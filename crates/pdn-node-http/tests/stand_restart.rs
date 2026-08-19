//! The stand's restart scenarios: a node's container stopped — cleanly and
//! by a kill with no grace — and started again on its own state directory,
//! asserted to come back as the same node; a kill landing in the middle of
//! a stream of acknowledged writes; and a state directory that fills.
//!
//! A restart is asserted here and nowhere else: an in-process respawn
//! proves nothing about a process that exits, and only a container carries
//! its filesystem across one.
//!
//! Ignored by default: the suite needs a container daemon and a built image,
//! and `just test-docker` builds the image and runs it.

use std::sync::Arc;

use anyhow::{ensure, Context as _, Result};
use axum::http::StatusCode;
use pdn_node::PdnId;
use pdn_node_http::shapes::{Connections, HostedIdentities, PeerGrants};

mod common;
use common::{body, entry_reads, grant_on, Host, Stand};

/// This node's wire id, read from the debug status page — the first line
/// is `node <id>`.
async fn node_id(host: &Host) -> Result<String> {
    let status = host.get("/debug/status").await?.ok()?;
    let text = String::from_utf8_lossy(&status);
    text.lines()
        .find_map(|line| line.strip_prefix("node "))
        .map(str::to_owned)
        .context("the status page carries no node line")
}

/// The recovery assertion set, identical for the clean-stop arm and the
/// kill arm on purpose: recovery that differed by the manner of stopping
/// would depend on the previous process having said goodbye, which a kill
/// does not provide. Same node id, the identity hosted, the connection
/// listed, `previous` — written and acknowledged before the stop — still
/// readable, and the counterparty converging on `sentinel`, written after
/// the restart with no invite minted and no linking performed.
async fn assert_recovered(
    issuer: &Host,
    id_before: &str,
    alice: PdnId,
    bob: PdnId,
    audience: &Host,
    previous: &'static [u8],
    sentinel: &'static [u8],
) -> Result<()> {
    ensure!(
        node_id(issuer).await? == id_before,
        "the node id must survive the restart"
    );
    let hosted: HostedIdentities = issuer.get("/debug/identities").await?.json()?;
    ensure!(
        hosted.identities.contains(&alice),
        "the identity must be hosted again: {hosted:?}"
    );
    let connections: Connections = issuer
        .get(&format!("/debug/identities/{alice}/connections"))
        .await?
        .json()?;
    ensure!(
        connections.connections.contains(&bob),
        "the connection must be listed again: {connections:?}"
    );
    // What was acknowledged before the stop reads back — polled, because
    // the data namespace re-binds on the armer's first sweep.
    entry_reads(issuer, alice, "contact/email", previous)
        .await
        .context("the entry acknowledged before the stop did not read back")?;
    // The recovered connection carries a fresh write to the counterparty —
    // no ceremony was repeated: nothing since the restart minted an invite
    // or linked a device.
    issuer
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(sentinel),
        )
        .await?
        .ok()?;
    entry_reads(audience, alice, "contact/email", sentinel)
        .await
        .context("the counterparty did not converge with the restarted node")?;
    // The grant published before the stop still stands on the audience.
    let grants: PeerGrants = audience
        .get(&format!("/debug/identities/{bob}/grants/{alice}"))
        .await?
        .json()?;
    ensure!(
        grants.grants.iter().any(|grant| grant.issuer == alice),
        "the grant must still be readable: {grants:?}"
    );
    Ok(())
}

/// The restart scenario with both arms and its denial: a node holding an
/// identity, a connection and a grant is stopped cleanly and started again,
/// then killed with no grace and started again — the same assertions both
/// times — and a node started from the same image on an empty state
/// directory holds none of it. Without that last arm the scenario would
/// pass just as well against a node that quietly re-created everything.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one scenario: both arms and the denial in the same place
async fn a_restarted_node_is_the_same_node_and_a_fresh_one_holds_nothing() -> Result<()> {
    let stand = Stand::new();
    let issuer = stand.spawn("issuer").await?;
    let audience = stand.spawn("audience").await?;

    let alice = issuer.create_identity().await?;
    let bob = audience.create_identity().await?;
    let payload = issuer
        .post(
            &format!("/debug/identities/{alice}/invite?lifetime_secs=120"),
            axum::body::Bytes::new(),
        )
        .await?
        .ok()?;
    audience
        .post(&format!("/debug/identities/{bob}/establish"), payload)
        .await?
        .ok()?;
    issuer
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"before the stop"),
        )
        .await?
        .ok()?;
    issuer
        .publish_grant(alice, bob, &grant_on(alice, "contact/email", false))
        .await?
        .ok()?;
    entry_reads(&audience, alice, "contact/email", b"before the stop")
        .await
        .context("the audience never read the granted entry before the stop")?;
    let id_before = node_id(&issuer).await?;

    // Arm one: the clean stop.
    issuer.stop().await?;
    ensure!(
        !issuer.is_running().await?,
        "the stopped container must be down before the start"
    );
    issuer.start().await?;
    assert_recovered(
        &issuer,
        &id_before,
        alice,
        bob,
        &audience,
        b"before the stop",
        b"after the stop",
    )
    .await
    .context("recovery after the clean stop")?;

    // Arm two: the kill — no grace, no shutdown path — with the same
    // assertions and no extra ones. The sentinel acknowledged in arm one
    // is what must have survived, and the kill waits out the stores'
    // settle window first: both stores commit on a timer after the
    // acknowledgement (the change's recorded finding), and a kill inside
    // that window takes the youngest acknowledged writes with it — the
    // mid-stream scenario below is where that window itself is the
    // subject.
    tokio::time::sleep(SETTLE_WINDOW).await;
    issuer.kill()?;
    ensure!(
        !issuer.is_running().await?,
        "the killed container must be down before the start"
    );
    issuer.start().await?;
    assert_recovered(
        &issuer,
        &id_before,
        alice,
        bob,
        &audience,
        b"after the stop",
        b"after the kill",
    )
    .await
    .context("recovery after the kill")?;

    // Denial: the same image on an empty state directory hosts nothing,
    // lists nothing, and refuses reads addressed to the restarted node's
    // identity.
    let fresh = stand.spawn("fresh").await?;
    ensure!(
        node_id(&fresh).await? != id_before,
        "a fresh directory must be a different node"
    );
    let hosted: HostedIdentities = fresh.get("/debug/identities").await?.json()?;
    ensure!(
        hosted.identities.is_empty(),
        "a fresh node must host nothing: {hosted:?}"
    );
    let refused = fresh
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?;
    ensure!(
        refused.status == StatusCode::CONFLICT,
        "a fresh node must refuse the identity as unknown, got {}: {}",
        refused.status,
        refused.text()
    );
    Ok(())
}

/// How long after its acknowledgement a write is committed for certain:
/// the stores commit after the acknowledgement, on a timer — the replica
/// store flushes its open write transaction every 500 milliseconds, and
/// the blob store's metadata commits when its batch closes — so a kill can
/// take acknowledged writes younger than this back to the last commit.
/// The two commit independently, so a kill inside the window can even keep
/// the entry record while losing its payload; the read then answers
/// absent, which is what "absent or whole, never torn" allows. Sized to
/// both timers with room for a loaded machine; tightening it is how this
/// scenario would catch a store whose commit cadence regressed.
const SETTLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// A kill in the middle of a stream of writes: every write acknowledged
/// before the stores' settle window reads back after the restart with its
/// payload, and a write the kill cut inside the window — acknowledged or
/// not — is absent or whole, never a torn value and never a read error.
/// Bounded durability and no tearing are what the stores provide (the
/// commit runs on a timer after the acknowledgement — the change's
/// recorded finding); this scenario asserts exactly that under fire, and
/// the stress pass hammers it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one kill, the settled and the cut writes in the same place
async fn a_kill_mid_stream_loses_nothing_settled() -> Result<()> {
    let stand = Stand::new();
    let node = Arc::new(stand.spawn("writer").await?);
    let alice = node.create_identity().await?;

    // The writer streams entries and records each acknowledgement's
    // instant until a write fails under it — the kill below lands while it
    // runs. Values differ per path, so a torn value cannot borrow a
    // neighbour's bytes.
    let acked: Arc<std::sync::Mutex<Vec<std::time::Instant>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = {
        let node = Arc::clone(&node);
        let acked = Arc::clone(&acked);
        tokio::spawn(async move {
            let mut index = 0usize;
            loop {
                let put = node
                    .put(
                        &format!("/debug/data/{alice}/stream/{index:04}"),
                        format!("value-{index:04}").into_bytes(),
                    )
                    .await;
                match put {
                    Ok(answer) if answer.status.is_success() => {
                        if let Ok(mut times) = acked.lock() {
                            times.push(std::time::Instant::now());
                        }
                        index += 1;
                    }
                    // The kill cut this write before its acknowledgement.
                    Ok(_) | Err(_) => return index,
                }
            }
        })
    };

    // The kill lands once the stream has flowed for longer than the settle
    // window, so writes older than the window demonstrably exist.
    let streaming_since = std::time::Instant::now();
    loop {
        let count = acked
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("ack log poisoned"))?
            .len();
        if count >= 10 && streaming_since.elapsed() > SETTLE_WINDOW * 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let killed_at = std::time::Instant::now();
    node.kill()?;
    let cut_at = writer.await?;
    let ack_times = acked
        .lock()
        .map_err(|_poisoned| anyhow::anyhow!("ack log poisoned"))?
        .clone();
    let settled = ack_times
        .iter()
        .filter(|instant| killed_at.duration_since(**instant) > SETTLE_WINDOW)
        .count();
    ensure!(
        settled >= 1,
        "at least one write must have settled before the kill"
    );

    node.start().await?;
    // Every settled write reads back whole. The first read waits out the
    // data namespace re-binding; the rest are local.
    for index in 0..settled {
        entry_reads(
            &node,
            alice,
            &format!("stream/{index:04}"),
            format!("value-{index:04}").as_bytes(),
        )
        .await
        .with_context(|| format!("entry {index} settled before the kill and must read"))?;
    }
    // Everything the kill cut inside the window — acknowledged writes
    // younger than it, and the one in flight — is absent or whole, never
    // partial and never an error the surface cannot name.
    for index in settled..=cut_at {
        let cut = node
            .get(&format!("/debug/data/{alice}/stream/{index:04}"))
            .await?;
        let whole =
            cut.status == StatusCode::OK && cut.body == format!("value-{index:04}").as_bytes();
        let absent = cut.status == StatusCode::NOT_FOUND;
        ensure!(
            whole || absent,
            "a write cut inside the settle window must be absent or whole, \
             got {} for {index}: {}",
            cut.status,
            cut.text()
        );
    }
    Ok(())
}

/// A storage failure arrives as a failure: a node whose state directory is
/// a size-bounded filesystem refuses a write once the store is full — as a
/// failed request, not a success — and a read of that path does not report
/// the value as stored. On an in-memory node a full disk is invisible; on
/// a device it is the ordinary way storage fails.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn a_full_state_directory_refuses_writes_loudly() -> Result<()> {
    let stand = Stand::new();
    // Room for the stores to provision and a handful of entries, small
    // enough that the fill below overruns it several times over.
    let node = stand
        .spawn_with_bounded_state("bounded", 16 * 1024 * 1024)
        .await?;
    let alice = node.create_identity().await?;

    // Each payload is distinct — the blob store is content-addressed, and
    // one payload written 256 times is one blob, which no bound ever meets.
    let payload_for = |index: usize| {
        let mut payload = vec![0xa5u8; 256 * 1024];
        payload.splice(..8, index.to_be_bytes());
        payload
    };
    let mut refused = None;
    // 256 writes of 256 KiB is four times the mount: the loop must meet
    // the refusal long before it runs out.
    for index in 0..256usize {
        let answer = node
            .put(
                &format!("/debug/data/{alice}/fill/{index:04}"),
                payload_for(index),
            )
            .await?;
        if !answer.status.is_success() {
            refused = Some((index, answer));
            break;
        }
    }
    let Some((refused_index, answer)) = refused else {
        anyhow::bail!("the store absorbed four times its filesystem's size without refusing");
    };
    ensure!(
        answer.status.is_client_error() || answer.status.is_server_error(),
        "the refusal must be an error status, got {}",
        answer.status
    );

    // The refused write is not stored: reading it back must not answer the
    // payload as if it were.
    let read = node
        .get(&format!("/debug/data/{alice}/fill/{refused_index:04}"))
        .await?;
    ensure!(
        !(read.status == StatusCode::OK && read.body == payload_for(refused_index)),
        "a refused write must not read back as stored"
    );
    Ok(())
}
