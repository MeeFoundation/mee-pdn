//! The facade's surface, driven through exported calls alone.
//!
//! Every arrange and act step here is the one an application performs: a
//! connection comes from a minted payload consumed by the other handle, a
//! granted namespace arrives because the runtime binds what a published
//! grant names, and a wait is a repeated read. No test body holds a ticket
//! — none crosses this surface — and none reaches around the facade into
//! the runtime's own services.
//!
//! Two handles are 2 nodes in one process, which is weaker than the
//! container stand and stronger than nothing: the runtime's own suites
//! prove the inter-node path and the stand proves it across processes, so
//! what these add is the surface.

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pdn_mobile::{claim_id, GrantedPath, PdnError, PdnNode};
use pdn_node::InvitePayload;
use tempfile::TempDir;
use test_utils::eventually;

/// A cadence a test can wait through: the periodic pass is what carries a
/// published grant record to the peer's copy of the connection's pair.
const CADENCE_SECS: u64 = 1;

/// The bound a linking act gets when a test performs one.
const LINK_BUDGET_SECS: u64 = 30;

/// How long a test waits for a stale invite to reach its outcome — a dial
/// at an address whose node came back with no secret pending.
const STALE_INVITE_BOUND: Duration = Duration::from_secs(45);

/// One handle, its own node, its own directory.
struct Device {
    node: Arc<PdnNode>,
    /// Held so the directory outlives the node that writes in it.
    _dir: TempDir,
}

impl Device {
    async fn boot() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let node = PdnNode::new()?;
        node.bring_up(path_of(&dir), CADENCE_SECS).await?;
        Ok(Self { node, _dir: dir })
    }
}

fn path_of(dir: &TempDir) -> String {
    dir.path().to_string_lossy().into_owned()
}

/// The headline claim of the whole surface, with the tightest denial this
/// surface can express beside it: a grantee reads exactly the claim it was
/// granted, a party connected to neither side obtains nothing, the
/// read-only claim refuses the grantee's write and leaves the value
/// standing, a withdrawal closes the access, and a re-grant over the same
/// claim reopens it.
///
/// The denial is in the same place as the positive on purpose. A read that
/// works proves nothing on its own — the outsider's refusal is what says
/// the grant is what opened the namespace.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one scenario, its denials in the same place
#[allow(clippy::similar_names)] // the roles of the scenario, named for what they are
async fn a_granted_claim_reads_for_the_grantee_and_for_nobody_else() -> Result<()> {
    let granter = Device::boot().await?;
    let grantee = Device::boot().await?;
    let outsider = Device::boot().await?;

    let alice = granter.node.create_identity().await?;
    let bob = grantee.node.create_identity().await?;
    let carol = outsider.node.create_identity().await?;

    // The connection is established from a payload, the way 2 people in a
    // room make one.
    let code = granter.node.mint_invite(alice.clone(), Some(120)).await?;
    grantee.node.consume_invite(bob.clone(), code).await?;
    assert_eq!(
        granter.node.connections(alice.clone()).await?,
        vec![bob.clone()],
        "the granter lists the peer it established with"
    );
    assert_eq!(
        grantee.node.connections(bob.clone()).await?,
        vec![alice.clone()]
    );

    granter
        .node
        .write_entry(
            alice.clone(),
            "contact/email".to_owned(),
            b"a@example".to_vec(),
        )
        .await?;
    granter
        .node
        .publish_grant(
            alice.clone(),
            bob.clone(),
            alice.clone(),
            vec![GrantedPath {
                path: "contact/email".to_owned(),
                write: false,
            }],
        )
        .await?;

    // What the granter published, read back from its own node rather than
    // remembered by the caller.
    let published = granter
        .node
        .read_own_grants(alice.clone(), bob.clone())
        .await?
        .ok_or_else(|| anyhow!("no grant published"))?;
    assert_eq!(
        published.issuer, alice,
        "the grant names the granter's data"
    );
    assert_eq!(published.audience, bob);
    let derived = claim_id(&alice, "contact/email")?;
    assert_eq!(
        published
            .claims
            .first()
            .map(|granted| granted.claim.clone()),
        Some(derived.clone()),
        "the claim a read reports is the derivation of the path granted"
    );
    assert_eq!(
        published.claims.first().map(|granted| granted.write),
        Some(false)
    );

    // The grantee waits by repeating the read, never by concluding from one
    // empty answer.
    let seen = eventually(|| async {
        let grants = grantee
            .node
            .read_peer_grants(bob.clone(), alice.clone())
            .await
            .map_err(anyhow::Error::new)?;
        Ok(grants
            .iter()
            .any(|grant| grant.claims.iter().any(|claim| claim.claim == derived)))
    })
    .await?;
    assert!(seen, "the published grant reached the grantee's node");

    let read = eventually(|| async {
        let value = grantee
            .node
            .read_entry(alice.clone(), "contact/email".to_owned())
            .await
            .map_err(anyhow::Error::new)?;
        Ok(value.as_deref() == Some(b"a@example".as_slice()))
    })
    .await?;
    assert!(read, "the granted claim reads back for the grantee");

    // The tightest denial this surface can stage: an identity connected to
    // neither side, probed after the grantee demonstrably read. The second
    // read negative — a party holding the replica's ticket and no
    // capability — cannot be staged here at all, because no ticket crosses
    // the facade; it stays with the runtime's own suites.
    let refused = outsider
        .node
        .read_entry(alice.clone(), "contact/email".to_owned())
        .await
        .expect_err("an outsider must not read the granter's data");
    assert!(
        matches!(refused, PdnError::UnknownIssuer { .. }),
        "the outsider is refused as addressing an issuer it holds nothing of: {refused:?}"
    );
    let listed = outsider
        .node
        .list_entries(alice.clone(), None)
        .await
        .expect_err("an outsider must not list the granter's entries");
    assert!(
        matches!(listed, PdnError::UnknownIssuer { .. }),
        "{listed:?}"
    );
    assert_eq!(
        outsider.node.connections(carol.clone()).await?,
        Vec::<String>::new(),
        "the outsider holds no connection to anyone"
    );

    // The read-only claim refuses the write, and the value that was there
    // stays there.
    let write = grantee
        .node
        .write_entry(
            alice.clone(),
            "contact/email".to_owned(),
            b"z@example".to_vec(),
        )
        .await
        .expect_err("a read-only claim must refuse a write");
    assert!(
        matches!(write, PdnError::WriteNotGranted { .. }),
        "the refusal names the ungranted write: {write:?}"
    );
    assert_eq!(
        grantee
            .node
            .read_entry(alice.clone(), "contact/email".to_owned())
            .await?
            .as_deref(),
        Some(b"a@example".as_slice()),
        "the value that was there before the refused write is still there"
    );

    // One grant record exists per granted issuer toward a peer, so a
    // publication replaces rather than accumulates: what the peer reads
    // after the second one is the second claim set, and the first claim is
    // no longer among what it may read.
    granter
        .node
        .write_entry(alice.clone(), "contact/phone".to_owned(), b"+1000".to_vec())
        .await?;
    granter
        .node
        .publish_grant(
            alice.clone(),
            bob.clone(),
            alice.clone(),
            vec![GrantedPath {
                path: "contact/phone".to_owned(),
                write: false,
            }],
        )
        .await?;
    let phone = claim_id(&alice, "contact/phone")?;
    let replaced = eventually(|| async {
        let grants = grantee
            .node
            .read_peer_grants(bob.clone(), alice.clone())
            .await
            .map_err(anyhow::Error::new)?;
        let claims: Vec<&str> = grants
            .iter()
            .flat_map(|grant| grant.claims.iter())
            .map(|claim| claim.claim.as_str())
            .collect();
        Ok(claims == vec![phone.as_str()])
    })
    .await?;
    assert!(
        replaced,
        "the second publication replaced the first rather than adding to it"
    );

    // The withdrawal closes the access: the grantee's node unbinds the
    // namespace, so a later read is refused rather than answered empty.
    granter
        .node
        .withdraw_grant(alice.clone(), bob.clone(), alice.clone())
        .await?;
    let closed = eventually(|| async {
        Ok(matches!(
            grantee
                .node
                .read_entry(alice.clone(), "contact/email".to_owned())
                .await,
            Err(PdnError::UnknownIssuer { .. })
        ))
    })
    .await?;
    assert!(closed, "the withdrawal closed the grantee's access");

    // And a re-grant over the same claim reopens it — a boundary rather
    // than a dead end.
    granter
        .node
        .publish_grant(
            alice.clone(),
            bob.clone(),
            alice.clone(),
            vec![GrantedPath {
                path: "contact/email".to_owned(),
                write: false,
            }],
        )
        .await?;
    let reopened = eventually(|| async {
        let value = grantee
            .node
            .read_entry(alice.clone(), "contact/email".to_owned())
            .await
            .unwrap_or(None);
        Ok(value.as_deref() == Some(b"a@example".as_slice()))
    })
    .await?;
    assert!(reopened, "the re-grant reopened the access it closed");

    for device in [granter, grantee, outsider] {
        device.node.stop().await?;
    }
    Ok(())
}

/// A listing reports metadata and matches whole path components, an
/// issuer this node holds nothing of is refused rather than answered
/// empty, and an entry's bytes cross unchanged in both directions.
#[tokio::test]
async fn entries_cross_as_bytes_and_list_by_component() -> Result<()> {
    let device = Device::boot().await?;
    let alice = device.node.create_identity().await?;

    let payload: Vec<u8> = (0..=255_u8).cycle().take(4096).collect();
    device
        .node
        .write_entry(alice.clone(), "contact/email".to_owned(), payload.clone())
        .await?;
    device
        .node
        .write_entry(
            alice.clone(),
            "contacts/email".to_owned(),
            b"other".to_vec(),
        )
        .await?;

    assert_eq!(
        device
            .node
            .read_entry(alice.clone(), "contact/email".to_owned())
            .await?,
        Some(payload.clone()),
        "what was written is what is read, with no framing of the facade's own"
    );

    let listed = device
        .node
        .list_entries(alice.clone(), Some("contact".to_owned()))
        .await?;
    let paths: Vec<&str> = listed.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["contact/email"],
        "a prefix matches whole components, not characters"
    );
    assert_eq!(
        listed.first().map(|entry| entry.payload_len),
        Some(u64::try_from(payload.len())?),
        "a listing reports the payload's length and fetches no payload"
    );

    // An issuer this node holds nothing of is a refusal, never an empty
    // listing: an empty answer would read as "that identity shares
    // nothing".
    let stranger = "a".repeat(64);
    let refused = device
        .node
        .list_entries(stranger, None)
        .await
        .expect_err("an unknown issuer must be refused");
    assert!(
        matches!(refused, PdnError::UnknownIssuer { .. }),
        "{refused:?}"
    );

    device.node.stop().await?;
    Ok(())
}

/// A payload minted here carries the runtime's own encoding, so a host of
/// another shape over the same runtime consumes what this one produced.
///
/// The bytes inside the code are what `pdn-node-http` takes as a request
/// body — the same serde encoding of the same runtime type. The assertion
/// goes through that encoding and back, and the payload it rebuilds is then
/// consumed for real: a divergence would otherwise be discovered by a
/// person in a room.
#[tokio::test]
async fn a_payload_crosses_in_the_runtimes_own_encoding() -> Result<()> {
    let inviter = Device::boot().await?;
    let scanner = Device::boot().await?;
    let alice = inviter.node.create_identity().await?;
    let bob = scanner.node.create_identity().await?;

    let code = inviter.node.mint_invite(alice.clone(), Some(120)).await?;
    let bytes = URL_SAFE_NO_PAD.decode(&code)?;
    let as_another_host_reads_it: InvitePayload = serde_json::from_slice(&bytes)?;
    assert_eq!(
        as_another_host_reads_it.inviter.to_string(),
        alice,
        "the payload names the inviting identity"
    );

    // Re-encoded by that other host's rules and consumed here: the round
    // trip is the interoperability, not the parse alone.
    let round_tripped = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&as_another_host_reads_it)?);
    scanner
        .node
        .consume_invite(bob.clone(), round_tripped)
        .await?;
    assert_eq!(scanner.node.connections(bob).await?, vec![alice]);

    for device in [inviter, scanner] {
        device.node.stop().await?;
    }
    Ok(())
}

/// A code read for the wrong act reaches the runtime and comes back as the
/// runtime's refusal, because nothing above the facade parses a payload to
/// tell the two acts apart.
#[tokio::test]
async fn a_code_read_for_the_wrong_act_is_refused_by_the_runtime() -> Result<()> {
    let inviter = Device::boot().await?;
    let scanner = Device::boot().await?;
    let alice = inviter.node.create_identity().await?;
    let bob = scanner.node.create_identity().await?;

    // A device-linking code, read on the screen that connects to a peer.
    let linking = inviter
        .node
        .mint_linking_payload(alice.clone(), Some(120))
        .await?;
    let wrong_act = scanner
        .node
        .consume_invite(bob.clone(), linking)
        .await
        .expect_err("an invite call must not consume a linking payload");
    assert!(
        matches!(wrong_act, PdnError::MalformedInput { .. }),
        "the two payload kinds do not decode as one another: {wrong_act:?}"
    );

    // And the other way round, with the budget the linking act takes.
    let invite = inviter.node.mint_invite(alice, Some(120)).await?;
    let wrong_act = scanner
        .node
        .consume_linking_payload(invite, LINK_BUDGET_SECS)
        .await
        .expect_err("a linking call must not consume an invite payload");
    assert!(
        matches!(wrong_act, PdnError::MalformedInput { .. }),
        "{wrong_act:?}"
    );
    assert_eq!(
        scanner.node.connections(bob).await?,
        Vec::<String>::new(),
        "neither refusal left a connection behind"
    );

    for device in [inviter, scanner] {
        device.node.stop().await?;
    }
    Ok(())
}

/// A bring-up nobody is waiting for any more leaves the handle usable, and
/// a stop that overtakes one leaves no node running.
///
/// Both are the same hazard from 2 sides: a shell that leaves the
/// foreground mid-bring-up, and a coroutine cancelled under it. Whichever
/// way the race falls, what must not happen is a handle stuck between its
/// states, or a node nobody holds keeping the directory — the next
/// bring-up would then be refused by a node its own caller had stopped.
#[tokio::test]
async fn a_bring_up_nobody_awaits_leaves_the_handle_usable() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // A stop issued while a bring-up is in flight.
    let node = PdnNode::new()?;
    let bringing = {
        let node = Arc::clone(&node);
        let path = path_of(&dir);
        tokio::spawn(async move { node.bring_up(path, CADENCE_SECS).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    node.stop().await?;
    let brought = bringing.await?;
    assert!(
        matches!(brought, Ok(()) | Err(PdnError::NodeNotUp)),
        "a bring-up a stop overtook either finished before it or reports the node not up: {brought:?}"
    );
    let after = node
        .create_identity()
        .await
        .expect_err("the stop must leave no node behind, whichever way the race fell");
    assert!(matches!(after, PdnError::NodeNotUp), "{after:?}");

    // And the directory is free, which is what says the overtaken node was
    // shut down rather than left running with nobody holding it.
    node.bring_up(path_of(&dir), CADENCE_SECS).await?;
    node.stop().await?;

    // A bring-up whose caller goes away.
    let abandoned = PdnNode::new()?;
    let cancelled = {
        let node = Arc::clone(&abandoned);
        let path = path_of(&dir);
        tokio::spawn(async move { node.bring_up(path, CADENCE_SECS).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancelled.abort();
    // The handle is not stranded mid-transition: a stop and a bring-up
    // reach a running node again.
    abandoned.stop().await?;
    abandoned.bring_up(path_of(&dir), CADENCE_SECS).await?;
    let identity = abandoned.create_identity().await?;
    assert!(
        !identity.is_empty(),
        "the handle came back to a running node"
    );
    abandoned.stop().await?;
    Ok(())
}

/// One handle owns one node: bring-up and stop are explicit, a stop is safe
/// to repeat, a second bring-up is refused rather than replacing the node,
/// a call with nothing up is refused as that, and a directory a running
/// node holds is refused as held.
#[tokio::test]
async fn one_handle_owns_one_node() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let node = PdnNode::new()?;

    let before = node
        .create_identity()
        .await
        .expect_err("a call before a bring-up must be refused");
    assert!(matches!(before, PdnError::NodeNotUp), "{before:?}");

    node.bring_up(path_of(&dir), CADENCE_SECS).await?;
    let again = node
        .bring_up(path_of(&dir), CADENCE_SECS)
        .await
        .expect_err("a second bring-up must be refused");
    assert!(matches!(again, PdnError::NodeAlreadyUp), "{again:?}");

    // A second handle on the same directory: one directory belongs to one
    // running node, and this is reported as that rather than as a corrupt
    // store.
    let sibling = PdnNode::new()?;
    let held = sibling
        .bring_up(path_of(&dir), CADENCE_SECS)
        .await
        .expect_err("a directory a running node holds must be refused");
    assert!(matches!(held, PdnError::StorageHeld), "{held:?}");

    node.stop().await?;
    node.stop().await?;
    let after = node
        .create_identity()
        .await
        .expect_err("a call after a stop must be refused");
    assert!(matches!(after, PdnError::NodeNotUp), "{after:?}");

    // With the first node down, the directory is free for the next one.
    sibling.bring_up(path_of(&dir), CADENCE_SECS).await?;
    sibling.stop().await?;
    Ok(())
}

/// A node comes back on its directory as itself: the same node id, the same
/// hosted identity, and its own entries readable at once with no ceremony
/// repeated. What does not come back is work in flight — an invite minted
/// before the stop is refused after it.
#[tokio::test]
async fn a_node_comes_back_on_its_directory() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let node = PdnNode::new()?;
    node.bring_up(path_of(&dir), CADENCE_SECS).await?;

    let alice = node.create_identity().await?;
    let node_id = node.node_id().await?;
    node.write_entry(
        alice.clone(),
        "contact/email".to_owned(),
        b"a@example".to_vec(),
    )
    .await?;
    let minted = node.mint_invite(alice.clone(), Some(600)).await?;

    node.stop().await?;
    node.bring_up(path_of(&dir), CADENCE_SECS).await?;

    assert_eq!(node.node_id().await?, node_id, "the same node answers");
    assert_eq!(
        node.hosted_identities().await?,
        vec![alice.clone()],
        "the identity it hosted is hosted again, with no ceremony repeated"
    );
    assert_eq!(
        node.read_entry(alice.clone(), "contact/email".to_owned())
            .await?
            .as_deref(),
        Some(b"a@example".as_slice()),
        "its own entries read at once"
    );

    // The invite was work in flight, and work in flight does not come back.
    // Which of the 3 ceremony outcomes it is depends on what the restart did
    // to the address the payload carries — the node id survives the
    // directory, the port the endpoint binds need not — so the assertion
    // names the set and excludes the 2 answers that would be wrong however
    // that went: a success, and an unrecognized failure.
    let scanner = Device::boot().await?;
    let bob = scanner.node.create_identity().await?;
    let stale = tokio::time::timeout(STALE_INVITE_BOUND, scanner.node.consume_invite(bob, minted))
        .await?
        .expect_err("an invite minted before the stop must not work after it");
    assert!(
        matches!(
            stale,
            PdnError::CeremonyRefused
                | PdnError::DialogueTimedOut
                | PdnError::CounterpartyUnreachable
        ),
        "the refusal is one the table names rather than an unrecognized failure: {stale:?}"
    );

    node.stop().await?;
    scanner.node.stop().await?;
    Ok(())
}
