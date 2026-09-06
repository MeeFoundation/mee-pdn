//! The stand's scenarios across containers, reached only over the
//! published HTTP port: no step reaches into a runtime, no namespace ticket
//! appears, and waiting for convergence is repeating the read. A ceremony
//! payload moves between nodes through the test, as a code moves between
//! two screens through a person. Ignored by default: `just test-docker`
//! builds the image and runs the suite.

use anyhow::{Context as _, Result};
use axum::{body::Bytes, http::StatusCode};
use pdn_node_http::shapes::{
    Connections, Entries, GrantCapability, GrantPublication, HostedIdentities, PeerGrants,
};

mod common;
use common::{
    body, claims_on, entry_answers, entry_reads, eventually, grant_on, own_grant_reads, Stand,
    CONVERGENCE_BUDGET,
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

    // The payload crosses as an opaque token, so this test never depends on
    // its fields. The lifetime is named explicitly.
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

    // The capability comes out of the poll, since a later read is a second
    // observation. Three guards keep a namespace ticket off this surface: a
    // field added to `GrantCapability` stops the conversion compiling, one
    // added and filled stops the destructuring below compiling, and
    // `deny_unknown_fields` refuses a response some other producer built —
    // the one that can fail at run time, with the message this decode
    // carries.
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
    let Some(capability) = capability else {
        anyhow::bail!(
            "the grant did not reach the grantee over the pair\n{}",
            scanner.diagnostics().await
        )
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

    // Denied (outsider): refused as unknown — a refusal, not an absence, so
    // a renamed route cannot pass.
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

    // Sentinel: a proven second wave orders the absence assertion below.
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

    // Denied (existence hidden).
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

    // Withdrawal: the binder forgets what the grant brought in, so the
    // issuer resolves to nothing there — a refusal, not an empty answer —
    // while the issuer keeps its own data.
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

/// A device joins an identity: the linking payload minted on the first
/// device is consumed on a second, which then hosts the identity and reads
/// what was written before it joined. Denied: the same payload presented
/// again is refused (its secret is burnt) and the presenter hosts nothing;
/// a node that never linked is refused as unknown — a node of its own,
/// since the bystander's refused attempt could leave a residue.
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
    // The budget of the whole act, named explicitly.
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

    // Denied (a replayed payload): a refusal, distinguishable from a node
    // that never reached the inviter.
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

    // Denied (a node that never linked).
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

/// A granted peer keeps converging after the publishing device is stopped:
/// the only place the issuer's whole device set is proven reachable across
/// processes, since a contact derived from a device record carries an
/// endpoint id alone. A failure to converge is not answered by a longer
/// budget: the waits before the stop rule out a sibling without the grant
/// record, and which peers the audience dialled comes out of its streamed
/// log (`grep -o "peer=[0-9a-f]*" <log> | sort | uniq -c`). The denial
/// beside it: the failover must not widen access.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a container daemon and the pdn-node-http:dev image (just test-docker)"]
#[allow(clippy::too_many_lines)] // one failover, with its denial in the same place
async fn a_stopped_device_does_not_stop_the_connection() -> Result<()> {
    let stand = Stand::new();
    let publisher = stand.spawn("alice-publisher").await?;
    let sibling = stand.spawn("alice-sibling").await?;
    let audience = stand.spawn("audience").await?;
    let outsider = stand.spawn("outsider").await?;

    // The second device joins by linking.
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

    // Read by the audience, so the failover starts from a connection that
    // demonstrably works.
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

    // Replication between the issuer's devices has run.
    entry_reads(
        &sibling,
        alice,
        "contact/email",
        b"published from the first device",
    )
    .await
    .context("the sibling never caught up on the granted claim before the stop")?;

    // And the sibling holds the grant record it serves by, which the claim
    // above does not imply: a device that never opened the pair reads the
    // claim while refusing the audience fail-closed.
    own_grant_reads(&sibling, alice, bob, alice)
        .await
        .context("the sibling never held the grant record before the stop")?;

    // Asked of the daemon, not the network: the released port can be
    // answered by a live node given the same port afterwards.
    publisher.stop().await?;
    assert!(
        !publisher.is_running().await?,
        "the device this scenario stops is still running"
    );

    // The audience converges on a device whose address it was never given.
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

    // Denied: the failover widened nothing.
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

/// A write grant lets the grantee write the claim it names, and the write
/// reaches the issuer. The tightest unauthorized party is this very peer,
/// one claim over: its write at the read-only claim of the same
/// publication is refused. That refusal proves the grantee-side courtesy
/// check only; the issuer's gate is proven in `pdn-node`'s
/// `scoped_writes.rs`, where the bypass lives.
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

    // The precondition of writing over it: the namespace is bound here.
    entry_reads(&grantee, alice, "contact/phone", b"+1-555-0100")
        .await
        .context("the granted entry did not reach the grantee")?;

    // Allowed: the value reads back on the issuer's side, so the write
    // crossed.
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

    // Sentinel: a completed session after the refusal, without which the
    // issuer-side read below would pass either way.
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

/// Two personas of one person on one node, each with an audience of its
/// own: sharing a process is not sharing an audience. The denials are read
/// from the peers' side, where each node hosts one identity and the
/// question is unambiguous — a read on Alice's node names the namespace and
/// never the reader. Not asserted: that one persona cannot read the other's
/// data on the node they share. Every enforcement point names the device,
/// so a device in two device sets resolves to both by design; no layer
/// draws that boundary.
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

    // Different data under the same path, so a read that reached the wrong
    // namespace answers the wrong bytes rather than nothing.
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

    // Read on Alice's node, where the route names which persona is asked.
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

    // Denied, both ways: refused as unknown rather than answered as absent.
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
