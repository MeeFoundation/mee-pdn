//! The retraction-marker record family in the private metadata store:
//! record, list (payload-waiting), and prune by issuer — the durable half
//! of a write-retraction verdict, keyed by granted issuer, author, and
//! path. Replication and consumption are proven end to end at the runtime
//! level (pdn-node's sibling-flap scenario); this pins the store surface,
//! plus the local-record check a verdict passes before it becomes a marker.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, AuthorId, PrivateMetadataStore, RetractionMarker, RetractionVerdict, ShareMode,
};
use iroh_blobs::Hash;
use pdn_types::{EntryPath, NodeId, PdnId};
use test_utils::{eventually, host_identity, ids, join_identity, memory_node};

fn marker(bound: u64) -> RetractionMarker {
    RetractionMarker {
        bound,
        decided_by: NodeId::from_bytes([7u8; 32]),
        content_hash: [3u8; 32],
        timestamp: bound,
    }
}

/// A marker round-trips by (issuer, author, path); a wider bound replaces a
/// narrower one; pruning by issuer drops exactly that issuer's markers and
/// leaves another issuer's standing.
#[tokio::test(flavor = "multi_thread")]
async fn markers_record_list_and_prune_by_issuer() -> Result<()> {
    let node = memory_node().await?;
    let pms = host_identity(&node, ids::ALICE).await?;
    let author = node.default_author(ids::ALICE)?;
    let other_issuer: PdnId = ids::CAROL;

    pms.record_retraction(ids::BOB, author, "contact/email", &marker(10))
        .await?;
    pms.record_retraction(other_issuer, author, "contact/phone", &marker(20))
        .await?;

    // Both markers list once their payloads are readable (local, so prompt).
    assert!(
        eventually(|| async { Ok(pms.list_retractions().await?.len() == 2) }).await?,
        "both markers must list"
    );
    let bob_marker = pms
        .list_retractions()
        .await?
        .into_iter()
        .find(|(issuer, _, path, _)| *issuer == ids::BOB && path == "contact/email")
        .expect("bob's marker lists");
    assert_eq!(bob_marker.3.bound, 10);
    assert_eq!(bob_marker.1, author);

    // A wider bound replaces the record wholesale.
    pms.record_retraction(ids::BOB, author, "contact/email", &marker(15))
        .await?;
    assert!(
        eventually(|| async {
            Ok(pms
                .list_retractions()
                .await?
                .into_iter()
                .any(|(issuer, _, path, m)| {
                    issuer == ids::BOB && path == "contact/email" && m.bound == 15
                }))
        })
        .await?,
        "the wider bound must replace the record"
    );

    // Pruning Bob's issuer drops his marker and leaves the other issuer's.
    pms.prune_retractions(ids::BOB).await?;
    assert!(
        eventually(|| async {
            let listed = pms.list_retractions().await?;
            Ok(listed.len() == 1 && listed[0].0 == other_issuer)
        })
        .await?,
        "prune must drop exactly Bob's markers"
    );

    node.shutdown().await?;
    Ok(())
}

/// The retention-window GC drops this device's aged markers and spares fresh
/// ones — aging is judged by the marker's PMS-entry timestamp, so it
/// holds whatever the marker's bound is.
#[tokio::test(flavor = "multi_thread")]
async fn aged_markers_are_pruned_by_the_retention_window() -> Result<()> {
    let node = memory_node().await?;
    let pms = host_identity(&node, ids::ALICE).await?;
    let author = node.default_author(ids::ALICE)?;

    pms.record_retraction(ids::BOB, author, "contact/email", &marker(10))
        .await?;
    assert!(
        eventually(|| async { Ok(pms.list_retractions().await?.len() == 1) }).await?,
        "the marker must list"
    );

    // Cutoff below the marker's write time (now == 0) spares it.
    assert!(pms.prune_aged_retractions(0, u64::MAX).await?.is_empty());
    assert_eq!(
        pms.list_retractions().await?.len(),
        1,
        "a fresh marker survives the GC"
    );

    // The window is subtracted, not ignored: a real `now` with an hour of
    // retention puts the cutoff an hour before the marker was written, and
    // the marker stands. A GC that pruned by `now` alone would take it.
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_micros(),
    )?;
    let hour = 60 * 60 * 1_000_000;
    assert!(pms.prune_aged_retractions(now, hour).await?.is_empty());
    assert_eq!(
        pms.list_retractions().await?.len(),
        1,
        "a marker younger than the retention window survives"
    );

    // The same `now` with no retention puts the cutoff at this instant, past
    // the marker's write time: it goes, and it is named back so the caller
    // can take down what it armed.
    let dropped = pms.prune_aged_retractions(now, 0).await?;
    assert_eq!(
        dropped,
        vec![(ids::BOB, author, "contact/email".to_owned())],
        "the pruned marker's address is reported"
    );
    assert!(
        eventually(|| async { Ok(pms.list_retractions().await?.is_empty()) }).await?,
        "an aged marker is pruned"
    );

    node.shutdown().await?;
    Ok(())
}

/// A marker at a path leaves the markers at longer paths standing, on the
/// device that recorded them and on its sibling.
#[tokio::test(flavor = "multi_thread")]
async fn a_marker_at_a_path_leaves_the_markers_at_longer_paths() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;
    let phone_pms = host_identity(&phone, ids::ALICE).await?;
    let ticket = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_pms = join_identity(&laptop, ids::ALICE, ticket).await?;
    phone_pms.add_device(laptop.node_id()).await?;
    laptop_pms.add_device(laptop.node_id()).await?;
    let author = phone.default_author(ids::ALICE)?;

    phone_pms
        .record_retraction(ids::BOB, author, "contact/email", &marker(10))
        .await?;
    phone_pms
        .record_retraction(ids::BOB, author, "contact", &marker(20))
        .await?;

    for (device, pms) in [("phone", &phone_pms), ("laptop", &laptop_pms)] {
        assert!(
            eventually(|| async {
                let mut paths: Vec<String> = pms
                    .list_retractions()
                    .await?
                    .into_iter()
                    .map(|(_issuer, _author, path, _marker)| path)
                    .collect();
                paths.sort_unstable();
                Ok(paths == ["contact", "contact/email"])
            })
            .await?,
            "both markers must list on the {device}"
        );
    }

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// Each marker as (issuer, recorded by `author`, path), sorted.
async fn listed(
    pms: &PrivateMetadataStore,
    author: AuthorId,
) -> Result<Vec<(PdnId, bool, String)>> {
    let mut listed: Vec<(PdnId, bool, String)> = pms
        .list_retractions()
        .await?
        .into_iter()
        .map(|(issuer, by, path, _marker)| (issuer, by == author, path))
        .collect();
    listed.sort_unstable();
    Ok(listed)
}

/// Pruning an issuer drops every marker this device recorded for it, nested
/// paths included, and leaves a sibling's marker for that issuer and this
/// device's marker for another issuer standing, on both devices.
///
/// The sibling's marker is the author scope: each device prunes the markers
/// it recorded, the unbind running on every device for itself.
#[tokio::test(flavor = "multi_thread")]
async fn pruning_an_issuer_drops_every_own_marker_and_spares_a_siblings() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;
    let phone_pms = host_identity(&phone, ids::ALICE).await?;
    let ticket = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_pms = join_identity(&laptop, ids::ALICE, ticket).await?;
    phone_pms.add_device(laptop.node_id()).await?;
    laptop_pms.add_device(laptop.node_id()).await?;
    let phone_author = phone.default_author(ids::ALICE)?;
    let laptop_author = laptop.default_author(ids::ALICE)?;

    for path in ["contact", "contact/email", "notes/x"] {
        phone_pms
            .record_retraction(ids::BOB, phone_author, path, &marker(10))
            .await?;
    }
    phone_pms
        .record_retraction(ids::CAROL, phone_author, "contact/phone", &marker(10))
        .await?;
    laptop_pms
        .record_retraction(ids::BOB, laptop_author, "notes/y", &marker(10))
        .await?;

    // Sentinel: every marker has replicated to the phone before it prunes.
    assert!(
        eventually(|| async { Ok(listed(&phone_pms, phone_author).await?.len() == 5) }).await?,
        "every marker must list on the phone"
    );

    phone_pms.prune_retractions(ids::BOB).await?;
    let mut standing = vec![
        (ids::BOB, false, "notes/y".to_owned()),
        (ids::CAROL, true, "contact/phone".to_owned()),
    ];
    standing.sort_unstable();
    for (device, pms) in [("phone", &phone_pms), ("laptop", &laptop_pms)] {
        assert!(
            eventually(|| async { Ok(listed(pms, phone_author).await? == standing) }).await?,
            "the {device} must list the sibling's marker and the other issuer's alone"
        );
    }

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// A verdict names an entry with fields the refusing peer chose, so the
/// local record is what makes them true: a timestamp above the record's, one
/// below it, a path never written and a foreign author each name nothing
/// here — a forged rejection retracts nothing.
///
/// The matching case is pinned where it acts: pdn-node's
/// `mixed_grant_email_read_only_phone_read_write` retracts a genuinely
/// refused entry, which this check must admit for that scenario to pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_verdict_naming_no_local_record_is_not_honored() -> Result<()> {
    let node = memory_node().await?;
    let _pms = host_identity(&node, ids::ALICE).await?;
    node.create_namespace(ids::ALICE, ids::BOB).await?;
    let author = node.default_author(ids::ALICE)?;
    let stranger = node.create_author(ids::ALICE).await?;
    let path = EntryPath::new("contact/email")?;
    let payload = b"alice@example.org";
    node.write(ids::ALICE, ids::BOB, author, &path, payload)
        .await?;
    let namespace = node
        .share_ticket(ids::ALICE, ids::BOB, ShareMode::Read, AddrInfoOptions::Id)
        .await?
        .capability
        .id();

    // Every field but the forged one is truthful, so each case fails on the
    // one thing it makes up.
    let hash = Hash::new(payload);
    let verdict = |author, key: &str, timestamp| RetractionVerdict {
        identity: ids::ALICE,
        namespace,
        author,
        key: key.as_bytes().to_vec(),
        timestamp,
        content_hash: hash,
    };
    let forged = [
        (
            verdict(author, path.as_str(), u64::MAX),
            "a timestamp above the record's",
        ),
        (
            verdict(author, path.as_str(), 0),
            "a timestamp below the record's",
        ),
        (
            verdict(author, "contact/phone", u64::MAX),
            "a path never written",
        ),
        (
            verdict(stranger, path.as_str(), u64::MAX),
            "another author at that path",
        ),
    ];
    for (verdict, what) in forged {
        assert!(
            !node
                .holds_rejected_entry(ids::ALICE, ids::BOB, &verdict)
                .await?,
            "{what} names no entry this node holds"
        );
    }

    node.shutdown().await?;
    Ok(())
}
