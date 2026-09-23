//! Two identities of one node meet inside the process: iroh refuses a
//! connection to the endpoint's own id, so what they exchange runs over a
//! pipe under the same protocol, the same classification and the same
//! egress filter as a session between two nodes (ADR-0013).
//!
//! Establishment lives in pdn-node; here the connection's metadata pair
//! and the grant record travel by the store-level acts the ceremony
//! performs.

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    claim_id_of, AddrInfoOptions, ConnectionMetadataStore, GrantedClaim, ReadGrant, ShareMode,
    SpawnOptions, SyncNode,
};
use pdn_types::{EntryPath, NonEmpty, PdnId};
use test_utils::{eventually, host_identity, ids};

const GRANTED: &str = "contact/email";
const WITHHELD: &str = "notes/diary";

/// A scoped holder has no gossip path, so a negative assertion is "the
/// reader retried over several passes and was refused"; at this cadence
/// that is milliseconds rather than the production default's tens of
/// seconds.
const RECONCILE: Duration = Duration::from_millis(500);

async fn spawn_node() -> Result<SyncNode> {
    SyncNode::spawn(SpawnOptions {
        reconcile_interval: RECONCILE,
        ..SpawnOptions::memory()
    })
    .await
}

/// One connected pair between two identities of one node, as
/// establishment leaves it: each side's own replica registered against the
/// other's, both naming this node as their device.
async fn connect_co_located(node: &SyncNode, left: PdnId, right: PdnId) -> Result<CoLocatedPair> {
    let left_own = ConnectionMetadataStore::create(node, left).await?;
    let right_own = ConnectionMetadataStore::create(node, right).await?;
    let left_read = left_own
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let right_read = right_own
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let right_as_left_sees = ConnectionMetadataStore::import(node, left, right_read).await?;
    let left_as_right_sees = ConnectionMetadataStore::import(node, right, left_read).await?;

    // Registered before either side publishes, the way the ceremony
    // leaves it: a record written into a replica no session classifies
    // yet reaches the other side only on a pass.
    node.host_connection(left, right, &left_own, &right_as_left_sees)?;
    node.host_connection(right, left, &right_own, &left_as_right_sees)?;
    left_own.publish_device(node.node_id()).await?;
    right_own.publish_device(node.node_id()).await?;
    Ok(CoLocatedPair {
        left_own,
        left_as_right_sees,
    })
}

/// The halves of the pair a scenario reaches for: the left identity's own
/// store, which is where it publishes, and the right identity's copy of
/// it, which is what admits the right identity's sessions.
struct CoLocatedPair {
    left_own: ConnectionMetadataStore,
    left_as_right_sees: ConnectionMetadataStore,
}

/// One identity of a node writes into a namespace a co-located identity
/// holds under a grant, and the granted claim reaches it with no other
/// node reachable — while the withheld claim stays absent.
///
/// Denied: a third identity of the same node, holding the very ticket the
/// grant carried, obtains nothing from it; and the grantee's own session
/// carries nothing of the withheld claim, whose absence a proven second
/// wave orders.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, the grant's reach and both denials in one place
async fn two_identities_of_one_node_converge_what_the_grant_covers() -> Result<()> {
    let node = spawn_node().await?;

    let work_dir = host_identity(&node, ids::ALICE_AT_WORK).await?;
    let _leisure_dir = host_identity(&node, ids::ALICE_AT_LEISURE).await?;
    let _outsider_dir = host_identity(&node, ids::CAROL).await?;

    // Work issues a namespace with one granted and one withheld claim.
    node.create_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await?;
    let author = node.default_author(ids::ALICE_AT_WORK)?;
    let granted_path = EntryPath::new(GRANTED)?;
    let withheld_path = EntryPath::new(WITHHELD)?;
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &granted_path,
        b"alice@work.example",
    )
    .await?;
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &withheld_path,
        b"dear diary",
    )
    .await?;
    let _work_devices = work_dir.list_devices().await?;

    let pair = connect_co_located(&node, ids::ALICE_AT_WORK, ids::ALICE_AT_LEISURE).await?;

    // The grant, with the ticket it carries — the grantee's whole path in.
    let ticket = node
        .share_ticket(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    let grant = ReadGrant {
        issuer: ids::ALICE_AT_WORK,
        audience: ids::ALICE_AT_LEISURE,
        claims: NonEmpty::new(GrantedClaim {
            claim: claim_id_of(&ids::ALICE_AT_WORK, &granted_path),
            write: false,
        }),
    };
    pair.left_own.publish_grant(&grant, &ticket).await?;
    node.import_namespace_scoped(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, ticket.clone())
        .await?;

    // Allowed: the granted claim crosses the process with no peer up.
    let arrived = eventually(|| async {
        Ok(node
            .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &granted_path)
            .await?
            .as_deref()
            == Some(b"alice@work.example".as_ref()))
    })
    .await?;
    assert!(
        arrived,
        "the granted claim did not reach the co-located identity"
    );

    // Denied (the withheld claim). Sentinel: a second granted value that
    // has crossed proves a further wave ran, so the absence below is a
    // refusal rather than a race.
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &granted_path,
        b"alice2@work.example",
    )
    .await?;
    assert!(
        eventually(|| async {
            Ok(node
                .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &granted_path)
                .await?
                .as_deref()
                == Some(b"alice2@work.example".as_ref()))
        })
        .await?,
        "the second wave did not cross"
    );
    assert_eq!(
        node.read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &withheld_path)
            .await?,
        None,
        "the withheld claim reached the co-located identity"
    );
    assert!(
        node.list(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, None)
            .await?
            .iter()
            .all(|entry| entry.path != withheld_path),
        "the withheld claim's existence reached the co-located identity"
    );

    // Denied (a co-located identity with the ticket and no grant). Several
    // reconcile passes: a scoped holder has no gossip path, so what it
    // would obtain it obtains on a pass.
    node.import_namespace_scoped(ids::CAROL, ids::ALICE_AT_WORK, ticket)
        .await?;
    tokio::time::sleep(RECONCILE * 6).await;
    assert_eq!(
        node.read(ids::CAROL, ids::ALICE_AT_WORK, &granted_path)
            .await?,
        None,
        "a co-located identity holding the ticket and no grant obtained the entry"
    );

    node.shutdown().await?;
    Ok(())
}

/// An identity of a node reads and lists nothing of an issuer only a
/// co-located identity holds, and is answered as it is for an issuer no
/// identity here holds.
#[tokio::test(flavor = "multi_thread")]
async fn an_issuer_a_co_located_identity_holds_is_unknown() -> Result<()> {
    let node = spawn_node().await?;
    let _work = host_identity(&node, ids::ALICE_AT_WORK).await?;
    let _leisure = host_identity(&node, ids::ALICE_AT_LEISURE).await?;

    node.create_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await?;
    let author = node.default_author(ids::ALICE_AT_WORK)?;
    let path = EntryPath::new(GRANTED)?;
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &path,
        b"alice@work.example",
    )
    .await?;

    let read = node
        .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &path)
        .await
        .expect_err("a co-located identity read an issuer it does not hold");
    assert!(
        read.downcast_ref::<data_layer::UnknownIssuer>().is_some(),
        "the refusal did not read as an unknown issuer: {read:#}"
    );
    let listed = node
        .list(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, None)
        .await
        .expect_err("a co-located identity listed an issuer it does not hold");
    assert!(
        listed.downcast_ref::<data_layer::UnknownIssuer>().is_some(),
        "the refusal did not read as an unknown issuer: {listed:#}"
    );

    // The same answer for an issuer no identity here holds at all.
    let absent = node
        .read(ids::ALICE_AT_LEISURE, ids::DAVE, &path)
        .await
        .expect_err("an issuer nobody holds was read");
    assert!(
        absent.downcast_ref::<data_layer::UnknownIssuer>().is_some(),
        "an unheld issuer answered differently: {absent:#}"
    );

    node.shutdown().await?;
    Ok(())
}

/// Two identities of one node granted by one issuer each write with their
/// own author, so an entry says which identity wrote it.
#[tokio::test(flavor = "multi_thread")]
async fn two_identities_of_one_node_write_as_two_authors() -> Result<()> {
    let node = spawn_node().await?;
    let _work = host_identity(&node, ids::ALICE_AT_WORK).await?;
    let _leisure = host_identity(&node, ids::ALICE_AT_LEISURE).await?;

    let work_author = node.default_author(ids::ALICE_AT_WORK)?;
    let leisure_author = node.default_author(ids::ALICE_AT_LEISURE)?;
    assert_ne!(
        work_author, leisure_author,
        "two identities of one node wrote under one author"
    );

    node.shutdown().await?;
    Ok(())
}

/// A co-located pair whose replicas differ is caught up by the periodic
/// pass over the pairs, and a pair that has converged is left alone: the
/// grantee holds no contact to dial and the grant is published after its
/// import, so neither a write announcement nor a dial of its own can be
/// what delivers; once delivered, the pair opens no further session over
/// several intervals.
///
/// Denied: before the grant is published the same import obtains nothing
/// over several passes, which is also what makes the delivery afterwards
/// the pass's doing.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, the catch-up and the quiet window in one place
async fn the_periodic_pass_catches_up_a_co_located_pair_and_leaves_a_quiet_one_alone() -> Result<()>
{
    let node = spawn_node().await?;
    let _work_dir = host_identity(&node, ids::ALICE_AT_WORK).await?;
    let _leisure_dir = host_identity(&node, ids::ALICE_AT_LEISURE).await?;

    // The namespace holds the granted claim alone, so a delivered
    // replica is an identical one and the pass has nothing left to do.
    node.create_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await?;
    let author = node.default_author(ids::ALICE_AT_WORK)?;
    let granted_path = EntryPath::new(GRANTED)?;
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &granted_path,
        b"alice@work.example",
    )
    .await?;

    let pair = connect_co_located(&node, ids::ALICE_AT_WORK, ids::ALICE_AT_LEISURE).await?;
    let ticket = node
        .share_ticket(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;

    // Denied, and the arrangement: the import and the dial it produces
    // happen while no grant covers them, and the grantee's contacts are
    // then emptied — the ticket named this node, and a contact that
    // resolves here is dialed every interval, which would deliver
    // whatever the pass would.
    node.import_namespace_scoped(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, ticket.clone())
        .await?;
    node.set_namespace_contacts(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, Vec::new())?;
    tokio::time::sleep(RECONCILE * 3).await;
    assert_eq!(
        node.read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &granted_path)
            .await?,
        None,
        "an import with no grant behind it obtained the entry"
    );

    // The grant alone, with no write into the namespace after it: what
    // delivers can only be a pass over a pair that differs.
    pair.left_own
        .publish_grant(
            &ReadGrant {
                issuer: ids::ALICE_AT_WORK,
                audience: ids::ALICE_AT_LEISURE,
                claims: NonEmpty::new(GrantedClaim {
                    claim: claim_id_of(&ids::ALICE_AT_WORK, &granted_path),
                    write: false,
                }),
            },
            &ticket,
        )
        .await?;
    assert!(
        eventually(|| async {
            Ok(node
                .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &granted_path)
                .await?
                .as_deref()
                == Some(b"alice@work.example".as_ref()))
        })
        .await?,
        "the periodic pass did not catch the co-located pair up"
    );

    // The pass judges the pair by what has moved since it last reconciled
    // it, so a delivery costs one more pass: the one that observes that
    // nothing moved after it. Wait for that to settle, then hold the pair
    // to silence. The metric counts the pass alone — a contact that
    // resolves to this node is dialed every interval, converged or not, as
    // any peer's would be.
    assert!(
        eventually(|| async {
            let before = node.co_located_pass_sessions();
            tokio::time::sleep(RECONCILE * 2).await;
            Ok(node.co_located_pass_sessions() == before)
        })
        .await?,
        "the pass never stopped reconciling the pair"
    );
    let before = node.co_located_pass_sessions();
    tokio::time::sleep(RECONCILE * 6).await;
    assert_eq!(
        node.co_located_pass_sessions(),
        before,
        "a pass over a converged co-located pair opened a session"
    );

    node.shutdown().await?;
    Ok(())
}

/// A grant widened while nothing is written reaches the co-located
/// audience: the claim it newly covers arrives although neither replica
/// changed between the two grants. What decides whether anything is owed
/// is the grant, and it lives in the connection stores — comparing the two
/// replicas cannot see it change, and comparing their author heads cannot
/// even see them differ here, since the audience holds the issuer's latest
/// entry and lacks only an earlier one.
///
/// Denied: before the widening the withheld claim stays absent over
/// several passes, which is also what makes its arrival afterwards the
/// widening's doing.
///
/// The pass's own decision is what the session count pins: it stands still
/// while nothing moves and moves once the grant does. Counted per
/// namespace, because publishing a grant writes into the pair's connection
/// stores and a total would move on their reconciliation alone.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, both grants and both waits in one place
async fn a_grant_widened_without_a_write_reaches_the_co_located_audience() -> Result<()> {
    let node = spawn_node().await?;
    let _work_dir = host_identity(&node, ids::ALICE_AT_WORK).await?;
    let _leisure_dir = host_identity(&node, ids::ALICE_AT_LEISURE).await?;

    node.create_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await?;
    let author = node.default_author(ids::ALICE_AT_WORK)?;
    let granted_path = EntryPath::new(GRANTED)?;
    let withheld_path = EntryPath::new(WITHHELD)?;

    // The withheld entry first: the audience ends up holding the issuer's
    // latest entry and lacking an earlier one, which is the arrangement in
    // which author heads of the two replicas are equal while their
    // contents are not.
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &withheld_path,
        b"the diary",
    )
    .await?;
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &granted_path,
        b"alice@work.example",
    )
    .await?;

    let pair = connect_co_located(&node, ids::ALICE_AT_WORK, ids::ALICE_AT_LEISURE).await?;
    let ticket = node
        .share_ticket(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    node.import_namespace_scoped(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, ticket.clone())
        .await?;
    node.set_namespace_contacts(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, Vec::new())?;

    let granted_claim = GrantedClaim {
        claim: claim_id_of(&ids::ALICE_AT_WORK, &granted_path),
        write: false,
    };
    pair.left_own
        .publish_grant(
            &ReadGrant {
                issuer: ids::ALICE_AT_WORK,
                audience: ids::ALICE_AT_LEISURE,
                claims: NonEmpty::new(granted_claim),
            },
            &ticket,
        )
        .await?;
    assert!(
        eventually(|| async {
            Ok(node
                .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &granted_path)
                .await?
                .as_deref()
                == Some(b"alice@work.example".as_ref()))
        })
        .await?,
        "the pass did not deliver the granted claim"
    );

    // Denied, and the sentinel: the withheld claim is absent over several
    // passes, so its arrival below cannot be the first grant's doing.
    tokio::time::sleep(RECONCILE * 3).await;
    assert_eq!(
        node.read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &withheld_path)
            .await?,
        None,
        "the withheld claim arrived under a grant that does not cover it"
    );

    // Quiet: nothing is written and no grant moves, so the pass opens
    // nothing — the reading it remembers is the one it took.
    let namespace = ticket.capability.id();
    let quiet = node.co_located_pass_sessions_of(namespace);
    tokio::time::sleep(RECONCILE * 3).await;
    assert_eq!(
        node.co_located_pass_sessions_of(namespace),
        quiet,
        "a pass over a pair where nothing moved opened a session"
    );

    // The widening alone: not one entry is written into the namespace
    // after it, on either side.
    pair.left_own
        .publish_grant(
            &ReadGrant {
                issuer: ids::ALICE_AT_WORK,
                audience: ids::ALICE_AT_LEISURE,
                claims: {
                    let mut claims = NonEmpty::new(granted_claim);
                    claims.push(GrantedClaim {
                        claim: claim_id_of(&ids::ALICE_AT_WORK, &withheld_path),
                        write: false,
                    });
                    claims
                },
            },
            &ticket,
        )
        .await?;
    assert!(
        eventually(|| async { Ok(node.co_located_pass_sessions_of(namespace) > quiet) }).await?,
        "the pass did not notice the grant it judges the pair by"
    );
    assert!(
        eventually(|| async {
            Ok(node
                .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &withheld_path)
                .await?
                .as_deref()
                == Some(b"the diary".as_ref()))
        })
        .await?,
        "the widened grant did not reach the co-located audience"
    );

    node.shutdown().await?;
    Ok(())
}

/// A write reaches a co-located holder of the namespace within one
/// reconcile interval, which is the announcement's doing: the write is
/// made right after a pass has demonstrably run, so the next one is a
/// whole interval away and what delivers inside the bound below cannot
/// be a pass.
///
/// Denied: the claim the grant withholds stays absent although the same
/// announcement is what opened the session that carried the granted one
/// — the announcement names a namespace and a holder, never an entry.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, the arrangement and both waves in one place
async fn a_write_reaches_a_co_located_holder_before_the_next_pass() -> Result<()> {
    // Long enough that the bound below sits well inside it, short enough
    // that the arrangement converges over a handful of passes.
    const INTERVAL: Duration = Duration::from_secs(3);
    /// What an announcement takes: a session over a pipe, microseconds.
    /// A third of the interval leaves the slop of observing a pass.
    const ANNOUNCED_WITHIN: Duration = Duration::from_secs(1);
    /// A pass opens one session per tracked replica in a burst; this is
    /// how long with none opened says the burst is over.
    const PASS_SETTLED: Duration = Duration::from_millis(300);

    let node = SyncNode::spawn(SpawnOptions {
        reconcile_interval: INTERVAL,
        ..SpawnOptions::memory()
    })
    .await?;
    let _work_dir = host_identity(&node, ids::ALICE_AT_WORK).await?;
    let _leisure_dir = host_identity(&node, ids::ALICE_AT_LEISURE).await?;

    node.create_namespace(ids::ALICE_AT_WORK, ids::ALICE_AT_WORK)
        .await?;
    let author = node.default_author(ids::ALICE_AT_WORK)?;
    let granted_path = EntryPath::new(GRANTED)?;
    let withheld_path = EntryPath::new(WITHHELD)?;
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &withheld_path,
        b"dear diary",
    )
    .await?;

    let pair = connect_co_located(&node, ids::ALICE_AT_WORK, ids::ALICE_AT_LEISURE).await?;
    let ticket = node
        .share_ticket(
            ids::ALICE_AT_WORK,
            ids::ALICE_AT_WORK,
            ShareMode::Read,
            AddrInfoOptions::RelayAndAddresses,
        )
        .await?;
    pair.left_own
        .publish_grant(
            &ReadGrant {
                issuer: ids::ALICE_AT_WORK,
                audience: ids::ALICE_AT_LEISURE,
                claims: NonEmpty::new(GrantedClaim {
                    claim: claim_id_of(&ids::ALICE_AT_WORK, &granted_path),
                    write: false,
                }),
            },
            &ticket,
        )
        .await?;
    node.import_namespace_scoped(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, ticket)
        .await?;

    // The arrangement converges over passes: the grant record has to
    // reach the grantee's copy of the pair before a session of its can
    // be admitted at all.
    assert!(
        eventually(|| async {
            Ok(pair
                .left_as_right_sees
                .read_grant(ids::ALICE_AT_WORK, ids::ALICE_AT_LEISURE)
                .await?
                .granted()
                .is_some())
        })
        .await?,
        "the grant record did not reach the co-located grantee's copy of the pair"
    );
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &granted_path,
        b"alice@work.example",
    )
    .await?;
    assert!(
        eventually(|| async {
            Ok(node
                .read(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &granted_path)
                .await?
                .as_deref()
                == Some(b"alice@work.example".as_ref()))
        })
        .await?,
        "the arrangement never converged — the premise of this test"
    );

    // The act, right after a pass: the grantee's replicas are dialed in
    // a burst once an interval, so the quiet that follows one burst puts
    // the next a whole interval away.
    assert!(
        eventually(|| async {
            let before = node.in_process_sessions(ids::ALICE_AT_LEISURE)?;
            tokio::time::sleep(PASS_SETTLED).await;
            Ok(node.in_process_sessions(ids::ALICE_AT_LEISURE)? == before)
        })
        .await?,
        "the grantee's sessions never quieted, so no interval starts here"
    );
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &granted_path,
        b"alice2@work.example",
    )
    .await?;
    node.write(
        ids::ALICE_AT_WORK,
        ids::ALICE_AT_WORK,
        author,
        &withheld_path,
        b"dear diary, again",
    )
    .await?;
    // Observed without the nudge a product read makes: a read of a
    // scoped replica pokes a reconciliation of its own, which would
    // deliver whatever the announcement would.
    let deadline = tokio::time::Instant::now() + ANNOUNCED_WITHIN;
    let mut announced = false;
    while tokio::time::Instant::now() < deadline {
        if node
            .read_unnudged(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &granted_path)
            .await?
            .as_deref()
            == Some(b"alice2@work.example".as_ref())
        {
            announced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        announced,
        "the write did not reach the co-located holder inside the interval"
    );
    assert_eq!(
        node.read_unnudged(ids::ALICE_AT_LEISURE, ids::ALICE_AT_WORK, &withheld_path)
            .await?,
        None,
        "the session an announcement opened carried the withheld claim across"
    );

    node.shutdown().await?;
    Ok(())
}
