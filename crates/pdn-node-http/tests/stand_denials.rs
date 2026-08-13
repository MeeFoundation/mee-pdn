//! The surface's refusals, each next to the authorized act it is the
//! tightest denial of ([access-control-tests]).
//!
//! Every one arrives as a client error with a status of its own, so a
//! container-level assertion can tell "the runtime refused you" from "you
//! asked wrong" and from "this host is broken". A surface that answered
//! alike for those would make each pairing below vacuous.
//!
//! An unhosted identity is 409 rather than the more natural 404 on purpose:
//! route names here are unpinned, so a test asserting absence would keep
//! passing after a route was renamed out from under it, and 404 stays
//! reserved for a route the host does not serve and for an entry that is
//! not there.
//!
//! [access-control-tests]: ../../../mia-docs/openspec/specs/code-practices/access-control-tests.md

use anyhow::{Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node::PdnId;
use pdn_node_http::shapes::{Connections, GrantPublication, HostedIdentities, PeerGrants};

mod common;
use common::{body, claims_on, entry_reads, grant_on, Stand};

/// An identity no runtime in this test creates or links.
const UNHOSTED: PdnId = PdnId::from_bytes([0x77; 32]);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // the denials of one surface, kept beside their positives
async fn refusals_arrive_as_refusals() -> Result<()> {
    let stand = Stand::new();
    let inviter = stand.spawn("inviter").await?;
    let scanner = stand.spawn("scanner").await?;
    let replayer = stand.spawn("replayer").await?;

    let alice = inviter.create_identity().await?;
    let bob = scanner.create_identity().await?;
    let carol = replayer.create_identity().await?;

    inviter
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"alice@example.org"),
        )
        .await?
        .ok()?;

    // Denied (an unhosted identity): the runtime does not host what the
    // request addressed. Beside it, the same route on a hosted identity
    // answers — without which "not the value I expected" would be all this
    // assertion says.
    let unhosted = inviter
        .get(&format!("/debug/identities/{UNHOSTED}/connections"))
        .await?;
    assert_eq!(
        unhosted.status,
        StatusCode::CONFLICT,
        "an unhosted identity must be refused, got {}: {}",
        unhosted.status,
        unhosted.text()
    );
    assert!(
        unhosted.text().contains(&UNHOSTED.to_string()),
        "the refusal must name what was addressed: {}",
        unhosted.text()
    );
    inviter
        .get(&format!("/debug/identities/{alice}/connections"))
        .await?
        .ok()?;

    // The connection the grant denials need. The payload is kept for the
    // replay below.
    let payload = inviter
        .post(&format!("/debug/identities/{alice}/invite"), Bytes::new())
        .await?
        .ok()?;
    scanner
        .post(
            &format!("/debug/identities/{bob}/establish"),
            payload.clone(),
        )
        .await?
        .ok()?;

    // Denied (a grant naming a foreign issuer): granting another identity's
    // data is delegation, which is not expressible. Beside it, the same
    // publication naming its own issuer is accepted.
    let foreign = inviter
        .publish_grant(
            alice,
            bob,
            &GrantPublication {
                issuer: bob,
                claims: claims_on("contact/email", false),
            },
        )
        .await?;
    assert_eq!(
        foreign.status,
        StatusCode::FORBIDDEN,
        "a grant naming a foreign issuer must be refused, got {}: {}",
        foreign.status,
        foreign.text()
    );
    let after_foreign: PeerGrants = inviter
        .get(&format!("/debug/identities/{alice}/grants/{bob}"))
        .await?
        .json()?;
    assert!(after_foreign.grants.is_empty());
    let mut allowed_claims = claims_on("contact/email", false);
    allowed_claims.extend(claims_on("contact/sentinel", false));
    inviter
        .publish_grant(
            alice,
            bob,
            &GrantPublication {
                issuer: alice,
                claims: allowed_claims,
            },
        )
        .await?
        .ok()?;

    // Denied (a grant naming no claim): every grant is claim-scoped, so an
    // empty claim set is a malformed request rather than a refusal.
    let claimless = inviter
        .publish_grant(
            alice,
            bob,
            &GrantPublication {
                issuer: alice,
                claims: Vec::new(),
            },
        )
        .await?;
    assert_eq!(
        claimless.status,
        StatusCode::BAD_REQUEST,
        "a claimless grant is malformed, got {}: {}",
        claimless.status,
        claimless.text()
    );

    // Denied (a replayed invite payload): the establishment above burnt the
    // secret, so presenting it again is refused — and the inviter records no
    // second connection, which is what says the refusal left no state.
    let refused_replay = replayer
        .post(&format!("/debug/identities/{carol}/establish"), payload)
        .await?;
    assert_eq!(
        refused_replay.status,
        StatusCode::FORBIDDEN,
        "a replayed invite payload must be refused, got {}: {}",
        refused_replay.status,
        refused_replay.text()
    );
    let recorded: Connections = inviter
        .get(&format!("/debug/identities/{alice}/connections"))
        .await?
        .json()?;
    assert_eq!(
        recorded.connections,
        vec![bob],
        "a refused establishment must leave no connection behind"
    );
    let replayer_recorded: Connections = replayer
        .get(&format!("/debug/identities/{carol}/connections"))
        .await?
        .json()?;
    assert!(replayer_recorded.connections.is_empty());

    // Allowed, then denied on the same claim: the grantee reads the granted
    // entry, and its write into the same claim is refused because the grant
    // covers it read-only — with the value that was there before still what
    // reads back on both sides.
    entry_reads(&scanner, alice, "contact/email", b"alice@example.org")
        .await
        .context("the granted entry did not reach the grantee")?;
    let refused_write = scanner
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"overwrite"),
        )
        .await?;
    assert_eq!(
        refused_write.status,
        StatusCode::FORBIDDEN,
        "a write outside the grant's write set must be refused, got {}: {}",
        refused_write.status,
        refused_write.text()
    );
    let grantee_side = scanner
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?
        .ok()?;
    assert_eq!(
        grantee_side,
        Bytes::from_static(b"alice@example.org"),
        "the refused write must not touch the grantee's own replica"
    );
    // Sentinel: a control write to another path proves a completed
    // two-way session ran after the refusal, before the issuer-side read
    // below is trusted to say anything about delivery rather than about
    // timing — an unpolled read right after the refusal could pass whether
    // or not the bad write was ever going to arrive.
    inviter
        .put(
            &format!("/debug/data/{alice}/contact/sentinel"),
            body(b"alice@new.example.org"),
        )
        .await?
        .ok()?;
    entry_reads(
        &scanner,
        alice,
        "contact/sentinel",
        b"alice@new.example.org",
    )
    .await
    .context("the sentinel update did not reach the grantee")?;
    let issuer_side = inviter
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?
        .ok()?;
    assert_eq!(
        issuer_side,
        Bytes::from_static(b"alice@example.org"),
        "the refused write must never reach the issuer"
    );

    // Denied (an absent entry): 404, the one status reserved for nothing
    // being there — the issuer is hosted, so this is not a refusal.
    let absent = inviter
        .get(&format!("/debug/data/{alice}/contact/phone"))
        .await?;
    assert_eq!(
        absent.status,
        StatusCode::NOT_FOUND,
        "an absent entry is 404, got {}: {}",
        absent.status,
        absent.text()
    );

    // Denied (a malformed identifier): the request itself is wrong, which is
    // neither a refusal nor an absence.
    let malformed = inviter.get("/debug/identities/not-hex/connections").await?;
    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "a malformed identifier is 400, got {}: {}",
        malformed.status,
        malformed.text()
    );

    // Denied (an empty payload): the engine keeps no zero-length entry, and
    // that is the request being wrong rather than the host not knowing what
    // happened. Beside it, one byte at the same path is accepted.
    let empty = inviter
        .put(&format!("/debug/data/{alice}/contact/note"), body(b""))
        .await?;
    assert_eq!(
        empty.status,
        StatusCode::BAD_REQUEST,
        "an empty payload is 400, got {}: {}",
        empty.status,
        empty.text()
    );
    inviter
        .put(&format!("/debug/data/{alice}/contact/note"), body(b"."))
        .await?
        .ok()?;

    // Denied (a mistyped query parameter): refused rather than ignored, so a
    // lifetime nobody asked for cannot be minted quietly. Beside it, the
    // spelled parameter is accepted.
    let mistyped = inviter
        .post(
            &format!("/debug/identities/{alice}/invite?lifetime_sec=60"),
            Bytes::new(),
        )
        .await?;
    assert_eq!(
        mistyped.status,
        StatusCode::BAD_REQUEST,
        "an unknown query parameter is 400, got {}: {}",
        mistyped.status,
        mistyped.text()
    );
    inviter
        .post(
            &format!("/debug/identities/{alice}/invite?lifetime_secs=60"),
            Bytes::new(),
        )
        .await?
        .ok()?;

    Ok(())
}

/// Denied (a peer with no connection metadata pair): a grant publish toward
/// an identity this issuer never established with, or received one from,
/// refuses as a conflict rather than a 500 — the runtime knows exactly why
/// (there is no pair to carry the grant), so the host must say so too.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn publishing_toward_an_unconnected_peer_is_refused_as_a_conflict() -> Result<()> {
    let stand = Stand::new();
    let inviter = stand.spawn("inviter").await?;
    let bystander = stand.spawn("bystander").await?;
    let alice = inviter.create_identity().await?;
    let bob = bystander.create_identity().await?;

    let refused = inviter
        .publish_grant(alice, bob, &grant_on(alice, "contact/email", false))
        .await?;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "a grant toward an unconnected peer must be refused, got {}: {}",
        refused.status,
        refused.text()
    );
    assert!(
        refused.text().contains(&bob.to_string()),
        "the refusal must name the unconnected peer: {}",
        refused.text()
    );

    Ok(())
}

/// Denied (linking into an already-hosted identity): `link`'s pre-dial
/// guard refuses before ever dialing, so a linking-invite payload for an
/// identity already hosted right here on the target is refused without
/// needing a reachable address.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn linking_into_an_already_hosted_identity_is_refused_as_a_conflict() -> Result<()> {
    let stand = Stand::new();
    let host = stand.spawn("host").await?;
    let alice = host.create_identity().await?;

    let self_invite = host
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    let already_hosted = host.post("/debug/link", self_invite).await?;
    assert_eq!(
        already_hosted.status,
        StatusCode::CONFLICT,
        "linking into an already-hosted identity must be refused, got {}: {}",
        already_hosted.status,
        already_hosted.text()
    );

    Ok(())
}

/// Denied (a concurrent link toward the same not-yet-hosted identity): two
/// `link` attempts racing toward two independent invites of the same
/// identity let exactly one commit — the loser refuses as a conflict
/// (another link already in flight, or, if the winner finished first,
/// already hosted) instead of both racing to register the identity twice.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn concurrent_links_toward_the_same_identity_let_only_one_commit() -> Result<()> {
    let stand = Stand::new();
    let inviter = stand.spawn("inviter").await?;
    let linker = stand.spawn("linker").await?;
    let alice = inviter.create_identity().await?;

    let first_invite = inviter
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    let second_invite = inviter
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;

    let (first, second) = tokio::join!(
        linker.post("/debug/link", first_invite),
        linker.post("/debug/link", second_invite)
    );
    let statuses = [first?.status, second?.status];
    assert!(
        statuses.contains(&StatusCode::NO_CONTENT) && statuses.contains(&StatusCode::CONFLICT),
        "exactly one of two concurrent links toward the same identity must succeed, got {statuses:?}"
    );
    let hosted: HostedIdentities = linker.get("/debug/identities").await?.json()?;
    assert_eq!(
        hosted
            .identities
            .iter()
            .filter(|identity| **identity == alice)
            .count(),
        1,
        "the concurrent loser must not leave duplicate or missing hosted state: {hosted:?}"
    );

    Ok(())
}
