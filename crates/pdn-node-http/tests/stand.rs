//! The stand's scenarios: identities meet, grant, replicate and withdraw
//! across containers, a device joins an identity, and a device goes away.
//!
//! Every node is its own process in its own container, reached only over its
//! published HTTP port. No step reaches into a runtime, and no namespace
//! ticket appears anywhere: a grantee reads because the runtime binds what
//! the grant names, which is the property the stand exists to demonstrate.
//! Waiting for convergence is repeating the read — the only means the
//! surface offers.
//!
//! What travels between the nodes is iroh, not HTTP: each request acts on
//! the runtime of the node serving it, and a ceremony payload moves between
//! them through the test — the caller — as a code moves between two screens
//! through a person.
//!
//! Ignored by default: the suite needs a container daemon and a built image,
//! and `just test-docker` builds the image and runs it.

use anyhow::{Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node_http::shapes::{
    Connections, Entries, GrantCapability, GrantPublication, HostedIdentities, PeerGrants,
};

mod common;
use common::{
    body, claims_on, entry_answers, entry_reads, eventually, grant_on, Stand, CONVERGENCE_BUDGET,
};

/// The whole stand scenario with its paired denials: two identities meet,
/// one grants a subset of its data, the grantee reads exactly that subset,
/// an outsider is refused, and the withdrawal closes the access the grant
/// opened.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one scenario, with its denials in the same place
async fn the_whole_scenario_runs_across_containers() -> Result<()> {
    let stand = Stand::new();
    let inviter = stand.spawn("inviter").await?;
    let scanner = stand.spawn("scanner").await?;
    let outsider = stand.spawn("outsider").await?;

    let alice = inviter.create_identity().await?;
    let bob = scanner.create_identity().await?;

    // Establishment. The payload crosses as an opaque token: the bytes one
    // node answered with, handed to the other unread, so this test never
    // comes to depend on which fields a payload has. The lifetime is named
    // explicitly; omitting it leaves the runtime's own short default.
    let payload = inviter
        .post(
            &format!("/debug/identities/{alice}/invite?lifetime_secs=120"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    scanner
        .post(&format!("/debug/identities/{bob}/establish"), payload)
        .await?
        .ok()?;

    // Both sides record the connection, each read from its own node.
    let inviter_side: Connections = inviter
        .get(&format!("/debug/identities/{alice}/connections"))
        .await?
        .json()?;
    assert!(
        inviter_side.connections.contains(&bob),
        "the inviter must record the connection: {inviter_side:?}"
    );
    let scanner_side: Connections = scanner
        .get(&format!("/debug/identities/{bob}/connections"))
        .await?
        .json()?;
    assert!(
        scanner_side.connections.contains(&alice),
        "the scanner must record the connection: {scanner_side:?}"
    );

    // Alice's data: the claim the grant will name, and one it will withhold.
    inviter
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"alice@example.org"),
        )
        .await?
        .ok()?;
    inviter
        .put(
            &format!("/debug/data/{alice}/notes/diary"),
            body(b"dear diary"),
        )
        .await?
        .ok()?;

    // The grant: read-only on exactly `contact/email`.
    inviter
        .publish_grant(alice, bob, &grant_on(alice, "contact/email", false))
        .await?
        .ok()?;

    // The capability comes out of the poll, not out of a read that follows
    // it: a record whose ticket payload is still arriving reads as no grant
    // at all — the transient this wait exists for — so a later read is a
    // second observation rather than the same one.
    //
    // Three guards keep a namespace ticket off this surface, and each closes
    // a different door. A field added to `GrantCapability` stops the
    // conversion from `ReadGrant` compiling; a field added and filled stops
    // the destructuring below compiling; and `deny_unknown_fields` refuses a
    // response some other producer built, which is the only one of the three
    // that can fail at run time — here, with the message this decode carries.
    let capability = eventually(CONVERGENCE_BUDGET, || async {
        let raw = scanner
            .get(&format!("/debug/identities/{bob}/grants/{alice}"))
            .await?
            .ok()?;
        let grants: PeerGrants = serde_json::from_slice(&raw).context(
            "the grants response carried an unexpected field — a namespace ticket, most likely",
        )?;
        Ok(grants
            .grants
            .into_iter()
            .find(|grant| grant.issuer == alice))
    })
    .await?;
    let capability = match capability {
        Some(capability) => capability,
        None => anyhow::bail!(
            "the grant did not reach the grantee over the pair\n{}",
            scanner.diagnostics().await
        ),
    };
    let GrantCapability {
        issuer: _,
        audience,
        claims,
    } = &capability;
    assert_eq!(*audience, bob);
    assert!(
        claims.iter().all(|claim| !claim.write),
        "the published grant is read-only: {capability:?}"
    );

    // Allowed: the granted entry reads back through the grantee, waited for
    // by repeating the read.
    entry_reads(&scanner, alice, "contact/email", b"alice@example.org")
        .await
        .context("the granted entry did not reach the grantee")?;

    // Denied (outsider): a node that never connected to Alice and holds no grant
    // is refused as unknown — a refusal, not an absence, so the assertion
    // cannot pass by way of a renamed route.
    let refused = outsider
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "an outsider must be refused as unknown, got {}: {}",
        refused.status,
        refused.text()
    );

    // Sentinel: an update to the granted claim proves a second replication
    // wave end to end, which is what orders the absence assertion below.
    inviter
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"alice@new.example.org"),
        )
        .await?
        .ok()?;
    entry_reads(&scanner, alice, "contact/email", b"alice@new.example.org")
        .await
        .context("the sentinel update did not reach the grantee")?;

    // Denied (existence hidden): after that wave, the grantee's view of
    // Alice's namespace carries exactly the granted claim.
    let listed: Entries = scanner.get(&format!("/debug/data/{alice}")).await?.json()?;
    let paths: Vec<String> = listed
        .entries
        .iter()
        .map(|entry| entry.path.to_string())
        .collect();
    assert_eq!(
        paths,
        vec!["contact/email".to_owned()],
        "the grantee's view must carry exactly the granted subset"
    );
    let withheld = scanner
        .get(&format!("/debug/data/{alice}/notes/diary"))
        .await?;
    assert_eq!(
        withheld.status,
        StatusCode::NOT_FOUND,
        "a withheld claim must read as absent, got {}: {}",
        withheld.status,
        withheld.text()
    );

    // Withdrawal, the counterpart of the grant above: the grantee's binder
    // forgets what the grant brought in, so the issuer resolves to nothing
    // there again — a refusal, not an empty answer. The issuer keeps its own
    // data throughout, which is what says withdrawal narrowed access and did
    // not delete anything.
    inviter
        .delete(&format!("/debug/identities/{alice}/grants/{bob}/{alice}"))
        .await?
        .ok()?;
    entry_answers(&scanner, alice, "contact/email", StatusCode::CONFLICT)
        .await
        .context("the withdrawn namespace stayed bound on the grantee")?;
    let after: PeerGrants = scanner
        .get(&format!("/debug/identities/{bob}/grants/{alice}"))
        .await?
        .json()?;
    assert!(
        after.grants.iter().all(|grant| grant.issuer != alice),
        "the withdrawn grant must be gone from the grantee's view: {after:?}"
    );
    let issuer_side = inviter
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?
        .ok()?;
    assert_eq!(
        issuer_side,
        Bytes::from_static(b"alice@new.example.org"),
        "withdrawal must leave the issuer's own entry untouched"
    );
    Ok(())
}

/// A device joins an identity: the linking payload is minted on the node of
/// the identity's first device and consumed on a second, which then reports
/// the identity among the ones it hosts and reads what was written before it
/// joined.
///
/// The paired denials sit beside the successful link: the same payload
/// presented a second time is refused — its secret is burnt — and the node
/// that presented it hosts nothing afterwards, and a node that never linked
/// is refused as unknown when it addresses the identity's namespace. The
/// stranger is a node of its own rather than the bystander, whose refused
/// attempt could leave a residue that passes the last check for the wrong
/// reason.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
async fn a_device_joins_across_containers() -> Result<()> {
    let stand = Stand::new();
    let first = stand.spawn("first").await?;
    let second = stand.spawn("second").await?;
    let bystander = stand.spawn("bystander").await?;
    let stranger = stand.spawn("stranger").await?;

    let alice = first.create_identity().await?;
    first
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"written before the link"),
        )
        .await?
        .ok()?;

    let payload = first
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    // The budget of the whole act — dialogue plus catch-up — named
    // explicitly; omitting it leaves the surface's own default.
    second
        .post("/debug/link?timeout_secs=60", payload.clone())
        .await?
        .ok()?;

    // The second node hosts the identity now.
    let hosted: HostedIdentities = second.get("/debug/identities").await?.json()?;
    assert!(
        hosted.identities.contains(&alice),
        "the linked node must report Alice: {hosted:?}"
    );

    // And reads what the first device wrote before it joined.
    entry_reads(&second, alice, "contact/email", b"written before the link")
        .await
        .context("the linked device did not catch up on the entry written before the link")?;

    // Denied (a replayed payload): the secret was burnt by the link above,
    // so a second presentation is refused — and the refusal is a refusal,
    // distinguishable from a node that never reached the inviter.
    let refused = bystander.post("/debug/link", payload).await?;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a replayed linking payload must be refused, got {}: {}",
        refused.status,
        refused.text()
    );
    let nothing: HostedIdentities = bystander.get("/debug/identities").await?.json()?;
    assert!(
        nothing.identities.is_empty(),
        "a refused link must leave nothing behind: {nothing:?}"
    );

    // Denied (a node that never linked): addressing the identity's namespace
    // is refused as unknown, not answered as absent.
    let outsider = stranger
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?;
    assert_eq!(
        outsider.status,
        StatusCode::CONFLICT,
        "a node that never linked must be refused as unknown, got {}: {}",
        outsider.status,
        outsider.text()
    );
    Ok(())
}

/// A granted peer keeps converging after the device that published the grant
/// is stopped: a sibling device of the same identity serves the namespace.
///
/// The property is that the issuer's whole device set is reachable, not only
/// the publishing one, and this is the only place it is proven across
/// processes: a contact derived from a device record carries an endpoint id
/// alone, and whether that resolves is a question about a real network.
///
/// A failure to converge after the stop is not answered by a longer budget,
/// and two causes produce it — told apart before either is acted on. One:
/// a device record carries an endpoint id alone, so a contact derived from
/// it does not resolve, which is a change of its own. Two: the sibling does
/// not hold the grant yet when the publisher stops. Nothing between the
/// publication and the stop asserts the second — the wait there is the
/// audience's read, and the audience and the sibling receive that record by
/// separate paths from the publisher.
///
/// The denial beside it: the failover must not widen access. A node that
/// never connected to the issuer is still refused afterwards, so "the peer
/// reads" cannot be satisfied by a node that serves whoever asks.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one failover, with its denial in the same place
async fn a_stopped_device_does_not_stop_the_connection() -> Result<()> {
    let stand = Stand::new();
    let publisher = stand.spawn("alice-publisher").await?;
    let sibling = stand.spawn("alice-sibling").await?;
    let audience = stand.spawn("audience").await?;
    let outsider = stand.spawn("outsider").await?;

    // The issuer on two devices: the second joins through the linking
    // ceremony, the way a device joins.
    let alice = publisher.create_identity().await?;
    let payload = publisher
        .post(
            &format!("/debug/identities/{alice}/linking-invite"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    sibling
        .post("/debug/link?timeout_secs=60", payload)
        .await?
        .ok()?;

    // The connection, established from the publishing device.
    let bob = audience.create_identity().await?;
    let invite = publisher
        .post(
            &format!("/debug/identities/{alice}/invite?lifetime_secs=120"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    audience
        .post(&format!("/debug/identities/{bob}/establish"), invite)
        .await?
        .ok()?;

    // The grant, published from that same device and read by the audience,
    // so the failover starts from a connection that demonstrably works.
    publisher
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"published from the first device"),
        )
        .await?
        .ok()?;
    publisher
        .publish_grant(alice, bob, &grant_on(alice, "contact/email", false))
        .await?
        .ok()?;
    entry_reads(
        &audience,
        alice,
        "contact/email",
        b"published from the first device",
    )
    .await
    .context("the audience never read the granted entry before the stop")?;

    // The device that published the grant goes away — and is gone: a device
    // still running would leave the convergence below provable by the very
    // one this scenario removes, which is the whole assertion. The daemon is
    // asked, not the network: the stopped container's published port is
    // released, and a probe to the address it used can be answered by a live
    // node that was given the same port afterwards.
    publisher.stop().await?;
    assert!(
        !publisher.is_running().await?,
        "the device this scenario stops is still running"
    );

    // The sibling writes, and the audience converges on it — reaching a
    // device of the issuer whose address it was never given.
    sibling
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"served by the sibling"),
        )
        .await?
        .ok()?;
    if let Err(err) = entry_reads(&audience, alice, "contact/email", b"served by the sibling").await
    {
        let logs = format!(
            "{}\n{}",
            audience.diagnostics().await,
            sibling.diagnostics().await
        );
        return Err(err.context(logs));
    }

    // Denied: the failover widened nothing. A node with no connection and no
    // grant is still refused as unknown, not answered as absent.
    let refused = outsider
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "an outsider must stay refused after the failover, got {}: {}",
        refused.status,
        refused.text()
    );
    Ok(())
}

/// A grant that names a claim writable lets the grantee write there, and the
/// write reaches the issuer: the granted peer is a writer of that claim, not
/// only a reader of it.
///
/// The write set is per claim, which is what the paired denial holds the
/// grant to. The same publication names a second path read-only, and the
/// grantee's write there is refused — the tightest unauthorized party for a
/// write is not an outsider but this very peer, one claim over. The refusal
/// is ordered against a later replication wave, so "the issuer never saw it"
/// says the write was rejected rather than that it had not arrived yet.
///
/// What that refusal proves is the courtesy check on the grantee's own side
/// (`write_refusal`, the sole caller of `covers_write`): the write never
/// leaves. The issuer's gate — `admit_ingest`, which derives its write set
/// independently — is the enforcement, and no test here reaches it: the
/// courtesy always answers first, and the only way past it is a runtime
/// feature this surface does not expose and should not. The gate is proven
/// where that bypass lives, in `pdn-node`'s `scoped_writes.rs`, which forces
/// a write outside the write set and asserts it never reaches the issuer and
/// that the provisional entry is retracted.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one write grant, with its denial in the same place
async fn a_write_grant_lets_the_grantee_write_what_it_names() -> Result<()> {
    let stand = Stand::new();
    let issuer = stand.spawn("issuer").await?;
    let grantee = stand.spawn("grantee").await?;

    let alice = issuer.create_identity().await?;
    let bob = grantee.create_identity().await?;

    let payload = issuer
        .post(
            &format!("/debug/identities/{alice}/invite?lifetime_secs=120"),
            Bytes::new(),
        )
        .await?
        .ok()?;
    grantee
        .post(&format!("/debug/identities/{bob}/establish"), payload)
        .await?
        .ok()?;

    // Alice's data: the claim the grant makes writable, and one it keeps
    // read-only.
    issuer
        .put(
            &format!("/debug/data/{alice}/contact/phone"),
            body(b"+1-555-0100"),
        )
        .await?
        .ok()?;
    issuer
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"alice@example.org"),
        )
        .await?
        .ok()?;

    // One publication, two claims, one of them writable.
    let mut claims = claims_on("contact/phone", true);
    claims.extend(claims_on("contact/email", false));
    issuer
        .publish_grant(
            alice,
            bob,
            &GrantPublication {
                issuer: alice,
                claims,
            },
        )
        .await?
        .ok()?;

    // The grantee holding the value is the precondition of writing over it:
    // it says the grant arrived and the namespace is bound here.
    entry_reads(&grantee, alice, "contact/phone", b"+1-555-0100")
        .await
        .context("the granted entry did not reach the grantee")?;

    // Allowed: the grantee writes the claim the grant made writable, and the
    // value reads back on both sides — on the issuer's, which is what says
    // the write crossed rather than stopping in the grantee's own replica.
    grantee
        .put(
            &format!("/debug/data/{alice}/contact/phone"),
            body(b"+1-555-0199"),
        )
        .await?
        .ok()?;
    entry_reads(&grantee, alice, "contact/phone", b"+1-555-0199")
        .await
        .context("the grantee's own write did not read back on the grantee")?;
    entry_reads(&issuer, alice, "contact/phone", b"+1-555-0199")
        .await
        .context("the grantee's write never reached the issuer")?;

    // Denied (one claim over): the same peer, under the same publication,
    // writing the claim that publication kept read-only.
    let refused = grantee
        .put(
            &format!("/debug/data/{alice}/contact/email"),
            body(b"bob@example.org"),
        )
        .await?;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "a write outside the grant's write set must be refused, got {}: {}",
        refused.status,
        refused.text()
    );

    // Sentinel: a write on the granted claim, observed arriving, proves a
    // completed session ran after the refusal — without it the issuer-side
    // read below would pass whether or not the refused write was ever going
    // to arrive.
    issuer
        .put(
            &format!("/debug/data/{alice}/contact/phone"),
            body(b"+1-555-0300"),
        )
        .await?
        .ok()?;
    entry_reads(&grantee, alice, "contact/phone", b"+1-555-0300")
        .await
        .context("the sentinel did not reach the grantee")?;

    let issuer_side = issuer
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?
        .ok()?;
    assert_eq!(
        issuer_side,
        Bytes::from_static(b"alice@example.org"),
        "the refused write must never reach the issuer"
    );
    let grantee_side = grantee
        .get(&format!("/debug/data/{alice}/contact/email"))
        .await?
        .ok()?;
    assert_eq!(
        grantee_side,
        Bytes::from_static(b"alice@example.org"),
        "the refused write must not touch the grantee's own replica"
    );
    Ok(())
}

/// Two personas of one person on one node, each with an audience of its own:
/// Alice at work is known to Bob, Alice at leisure to Carol, and both
/// connections carry data.
///
/// The node hosts both identities, which is the one arrangement the rest of
/// this suite never builds — every other test puts one identity on one
/// container. What it holds is that sharing a process is not sharing an
/// audience: connections and grants are keyed by the hosting identity, so
/// Bob is a peer of the work persona and a stranger to the leisure one.
///
/// The paired denials are read from the peers' side on purpose. There each
/// node hosts a single identity, so "Bob asks for the leisure namespace" is
/// an unambiguous question; asked on Alice's own node it would not be, since
/// the read names the namespace and never the reader. Both denials are
/// ordered after both positive reads, so an absence cannot pass for a value
/// that has not replicated yet.
///
/// What no test here asserts is the other half of co-location: that one
/// persona cannot read the other's data on the node they share. The
/// principal every enforcement point names is the device — a serving node
/// resolves a caller's rights from its transport-authenticated node id
/// through the published device sets, the ingest gate keys write admission
/// by namespace and node id, and a namespace secret is a bearer ticket
/// scoped to neither persona. A device publishing one node id in two device
/// sets therefore resolves to both, and its rights are the union by design.
/// An assertion here would name a boundary that no layer draws, and the
/// surface it would be written against — a read that names the namespace and
/// never the reader — is the shape of that same fact, not its cause.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // two personas and two audiences, kept in one place
async fn two_personas_on_one_node_keep_separate_audiences() -> Result<()> {
    let stand = Stand::new();
    let alice_node = stand.spawn("alice").await?;
    let bob_node = stand.spawn("bob").await?;
    let carol_node = stand.spawn("carol").await?;

    // One node, two identities.
    let at_work = alice_node.create_identity().await?;
    let at_leisure = alice_node.create_identity().await?;
    assert_ne!(at_work, at_leisure);
    let hosted: HostedIdentities = alice_node.get("/debug/identities").await?.json()?;
    assert!(
        hosted.identities.contains(&at_work) && hosted.identities.contains(&at_leisure),
        "the node must report both personas: {hosted:?}"
    );

    let bob = bob_node.create_identity().await?;
    let carol = carol_node.create_identity().await?;

    // Each persona meets its own peer.
    for (persona, peer_node, peer) in [(at_work, &bob_node, bob), (at_leisure, &carol_node, carol)]
    {
        let payload = alice_node
            .post(
                &format!("/debug/identities/{persona}/invite?lifetime_secs=120"),
                Bytes::new(),
            )
            .await?
            .ok()?;
        peer_node
            .post(&format!("/debug/identities/{peer}/establish"), payload)
            .await?
            .ok()?;
    }

    // The same path under each persona, holding different data — so a read
    // that reached the wrong namespace would answer the wrong bytes rather
    // than nothing.
    alice_node
        .put(
            &format!("/debug/data/{at_work}/contact/email"),
            body(b"alice@acme.example"),
        )
        .await?
        .ok()?;
    alice_node
        .put(
            &format!("/debug/data/{at_leisure}/contact/email"),
            body(b"alice@bridgeclub.example"),
        )
        .await?
        .ok()?;

    alice_node
        .publish_grant(at_work, bob, &grant_on(at_work, "contact/email", false))
        .await?
        .ok()?;
    alice_node
        .publish_grant(
            at_leisure,
            carol,
            &grant_on(at_leisure, "contact/email", false),
        )
        .await?
        .ok()?;

    // Allowed, both ways: each peer reads its own persona's data.
    entry_reads(&bob_node, at_work, "contact/email", b"alice@acme.example")
        .await
        .context("Bob did not read the work persona's entry")?;
    entry_reads(
        &carol_node,
        at_leisure,
        "contact/email",
        b"alice@bridgeclub.example",
    )
    .await
    .context("Carol did not read the leisure persona's entry")?;

    // Each persona's connections carry its own peer and not the other's —
    // read on Alice's node, where the route names which persona is asked.
    let work_side: Connections = alice_node
        .get(&format!("/debug/identities/{at_work}/connections"))
        .await?
        .json()?;
    assert!(
        work_side.connections.contains(&bob) && !work_side.connections.contains(&carol),
        "the work persona knows Bob and not Carol: {work_side:?}"
    );
    let leisure_side: Connections = alice_node
        .get(&format!("/debug/identities/{at_leisure}/connections"))
        .await?
        .json()?;
    assert!(
        leisure_side.connections.contains(&carol) && !leisure_side.connections.contains(&bob),
        "the leisure persona knows Carol and not Bob: {leisure_side:?}"
    );

    // Denied, both ways: a peer of one persona is a stranger to the other,
    // and is refused as unknown rather than answered as absent.
    for (peer_node, other_persona, who) in [
        (&bob_node, at_leisure, "Bob"),
        (&carol_node, at_work, "Carol"),
    ] {
        let refused = peer_node
            .get(&format!("/debug/data/{other_persona}/contact/email"))
            .await?;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{who} must be refused the other persona's namespace, got {}: {}",
            refused.status,
            refused.text()
        );
    }
    Ok(())
}
