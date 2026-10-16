//! Two devices of one identity (Alice) replicate her PMS. Both nodes host
//! Alice the way the product does — her PMS naming the device — though a
//! PMS session is served on its ticket alone (Invariant 1).

use std::time::Duration;

use anyhow::Result;
use data_layer::{
    pod_ticket_kind, AddrInfoOptions, CatchUpTimeout, DocTicket, PodStore, PrivateMetadataStore,
    RecordKey, Seq, ShareMode, SyncNode,
};
use pdn_types::{PodId, RecordId};
use test_utils::{eventually, host_identity, ids, join_identity, memory_node, TIMEOUT};

const FAMILY: PodId = PodId::from_bytes([0xfa; 16]);

/// Whether `pms` comes to read `want` as `pod`'s record.
async fn wait_pod_record(
    pms: &PrivateMetadataStore,
    pod: PodId,
    want: Option<(Seq, bool)>,
) -> Result<bool> {
    eventually(|| async { Ok(pms.pod_record(pod).await? == want) }).await
}

/// Whether `pms` comes to read `want` under `kind`.
async fn wait_ticket(pms: &PrivateMetadataStore, kind: &str, want: &DocTicket) -> Result<bool> {
    eventually(|| async {
        Ok(pms
            .get_ticket(kind)
            .await?
            .is_some_and(|got| got.to_string() == want.to_string()))
    })
    .await
}

/// A ticket of its own replica, so each write of a test carries a payload
/// no other write does.
async fn fresh_ticket(node: &SyncNode) -> Result<DocTicket> {
    PrivateMetadataStore::create(node, ids::ALICE)
        .await?
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await
}

/// Alice's phone and laptop, each hosting her and listing the laptop as her
/// device, as linking leaves them.
async fn two_devices() -> Result<(
    SyncNode,
    PrivateMetadataStore,
    SyncNode,
    PrivateMetadataStore,
)> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;
    let phone_pms = host_identity(&phone, ids::ALICE).await?;
    let ticket = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_pms = join_identity(&laptop, ids::ALICE, ticket).await?;
    phone_pms.add_device(laptop.node_id()).await?;
    laptop_pms.add_device(laptop.node_id()).await?;
    Ok((phone, phone_pms, laptop, laptop_pms))
}

/// Alice's PMS replicates to the laptop: a pod record written before the
/// import arrives as catch-up, and the pod's tombstone and a ticket written
/// after it arrive live.
#[tokio::test(flavor = "multi_thread")]
async fn sync_two_devices() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    let phone_pms = host_identity(&phone, ids::ALICE).await?;
    phone_pms.record_pod(FAMILY, Seq::FIRST).await?;

    let pms_ticket = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_pms = join_identity(&laptop, ids::ALICE, pms_ticket).await?;
    phone_pms.add_device(laptop.node_id()).await?;
    laptop_pms.add_device(laptop.node_id()).await?;

    assert!(
        wait_pod_record(&laptop_pms, FAMILY, Some((Seq::FIRST, true))).await?,
        "laptop did not catch up the pod record from phone"
    );

    phone_pms.tombstone_pod(FAMILY, Seq::new(2)).await?;
    assert!(
        wait_pod_record(&laptop_pms, FAMILY, Some((Seq::new(2), false))).await?,
        "laptop did not observe the pod's tombstone from phone"
    );

    let kind = pod_ticket_kind(&FAMILY, PodStore::Records);
    let ticket = fresh_ticket(&phone).await?;
    phone_pms.put_ticket(&kind, &ticket).await?;
    assert!(
        wait_ticket(&laptop_pms, &kind, &ticket).await?,
        "laptop did not observe the live ticket from phone"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// Concurrent ticket updates of one kind on both devices converge: both
/// replicas end up reading the same ticket, one of the two written. Which
/// write wins is decided by timestamps and is not asserted.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writes_converge() -> Result<()> {
    let (phone, phone_pms, laptop, laptop_pms) = two_devices().await?;
    let kind = pod_ticket_kind(&FAMILY, PodStore::Membership);
    let from_phone = fresh_ticket(&phone).await?;
    let from_laptop = fresh_ticket(&laptop).await?;

    phone_pms.put_ticket(&kind, &from_phone).await?;
    laptop_pms.put_ticket(&kind, &from_laptop).await?;

    // Fences: once each side reads the other's fence, sessions have run in
    // both directions and had the chance to carry the contested kind too.
    phone_pms.put_ticket("fence/phone", &from_phone).await?;
    laptop_pms.put_ticket("fence/laptop", &from_laptop).await?;
    assert!(
        wait_ticket(&laptop_pms, "fence/phone", &from_phone).await?,
        "phone's fence did not reach laptop"
    );
    assert!(
        wait_ticket(&phone_pms, "fence/laptop", &from_laptop).await?,
        "laptop's fence did not reach phone"
    );

    let converged = eventually(|| async {
        let on_phone = phone_pms.get_ticket(&kind).await?.map(|t| t.to_string());
        let on_laptop = laptop_pms.get_ticket(&kind).await?.map(|t| t.to_string());
        Ok(on_phone.is_some() && on_phone == on_laptop)
    })
    .await?;
    assert!(converged, "replicas did not converge on the contested kind");
    let value = phone_pms
        .get_ticket(&kind)
        .await?
        .expect("converged")
        .to_string();
    assert!(
        value == from_phone.to_string() || value == from_laptop.to_string(),
        "converged to a ticket neither device wrote"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// A write at a shorter key leaves the entries at longer keys sharing its
/// components or its bytes standing, on the writing device and on its
/// sibling.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_at_a_shorter_key_leaves_the_longer_ones_standing() -> Result<()> {
    let (phone, phone_pms, laptop, laptop_pms) = two_devices().await?;

    let mut written = Vec::new();
    for kind in ["contact/email", "contacts/emergency", "contact"] {
        let ticket = fresh_ticket(&phone).await?;
        phone_pms.put_ticket(kind, &ticket).await?;
        written.push((kind, ticket));
    }

    for pms in [&phone_pms, &laptop_pms] {
        for (kind, ticket) in &written {
            assert!(
                wait_ticket(pms, kind, ticket).await?,
                "{kind} does not read what was written at it"
            );
        }
        let mut listed = pms.list_ticket_kinds().await?;
        listed.retain(|kind| kind.starts_with("contact"));
        listed.sort_unstable();
        assert_eq!(listed, ["contact", "contact/email", "contacts/emergency"]);
    }

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// A ticket published on one device becomes readable on another once its
/// payload arrives, equal to the one published (`get_ticket` is `None` on
/// the record alone).
#[tokio::test(flavor = "multi_thread")]
async fn pms_carries_arbitrary_tickets() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    let phone_pms = host_identity(&phone, ids::ALICE).await?;
    let kind = pod_ticket_kind(&FAMILY, PodStore::Records);
    let payload_ticket = fresh_ticket(&phone).await?;
    phone_pms.put_ticket(&kind, &payload_ticket).await?;

    let ticket = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    let laptop_pms = join_identity(&laptop, ids::ALICE, ticket).await?;
    phone_pms.add_device(laptop.node_id()).await?;

    let arrived =
        eventually(|| async { Ok(laptop_pms.get_ticket(&kind).await?.is_some()) }).await?;
    assert!(arrived, "the published ticket did not become readable");
    let got = laptop_pms.get_ticket(&kind).await?.expect("just observed");
    assert_eq!(
        got.to_string(),
        payload_ticket.to_string(),
        "the ticket round-tripped with a different value"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// The PMS's catch-up wait returns on a completed sync session, not
/// on arrived content: the first wait, watched from before the arming,
/// covers the arming's first session (and the content it carried), and a
/// second wait from a fresh instant — after
/// which no new content will ever arrive — still returns, woken by a later
/// session that found nothing new. A content poll cannot see that session;
/// the wait must.
#[tokio::test(flavor = "multi_thread")]
async fn pms_wait_returns_on_a_session_not_on_content() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    let phone_pms = host_identity(&phone, ids::ALICE).await?;
    phone_pms.record_pod(FAMILY, Seq::FIRST).await?;
    let ticket = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    laptop.provision_identity(ids::ALICE).await?;
    let laptop_pms = PrivateMetadataStore::import(&laptop, ids::ALICE, ticket).await?;
    let catch_up = laptop_pms.watch_catch_up().await?;
    laptop.host_identity(ids::ALICE, &laptop_pms)?;
    catch_up.wait(TIMEOUT).await?;
    // The session that returned the wait carried the pre-import record.
    assert_eq!(
        laptop_pms.pod_record(FAMILY).await?,
        Some((Seq::FIRST, true)),
        "a successful catch-up session must have carried the existing records"
    );

    // From a fresh instant nothing new will arrive — the wait returns on
    // the next completed session alone (the node's periodic reconcile pass).
    laptop_pms.watch_catch_up().await?.wait(TIMEOUT).await?;

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

/// A PMS whose only peer is gone cannot catch up: the wait fails with
/// the distinguishable timeout, not a hang and not a success.
#[tokio::test(flavor = "multi_thread")]
async fn pms_wait_times_out_without_a_reachable_peer() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;

    let phone_pms = host_identity(&phone, ids::ALICE).await?;
    let ticket = phone_pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    // The ticket's only contact goes away before the import.
    phone.shutdown().await?;

    laptop.provision_identity(ids::ALICE).await?;
    let laptop_pms = PrivateMetadataStore::import(&laptop, ids::ALICE, ticket).await?;
    let catch_up = laptop_pms.watch_catch_up().await?;
    laptop.host_identity(ids::ALICE, &laptop_pms)?;
    let err = catch_up.wait(Duration::from_secs(2)).await.unwrap_err();
    assert!(
        err.downcast_ref::<CatchUpTimeout>().is_some(),
        "expected the typed catch-up timeout, got: {err:#}"
    );

    laptop.shutdown().await?;
    Ok(())
}

/// An empty payload is not a storable value: zero-length entries are the
/// store's deletion marker, and writing one into a pod's record store is
/// refused — it neither stores an empty value nor deletes the previous one.
#[tokio::test(flavor = "multi_thread")]
async fn empty_payload_write_is_rejected() -> Result<()> {
    let node = memory_node().await?;
    let (alice, _pms) = test_utils::pod::host(&node).await?;
    let pod = test_utils::pod::create(&node, &alice).await?;
    let key = RecordKey::Claim {
        member: alice.id,
        id: RecordId::from_bytes([0x07; 16]),
        mseq: Seq::FIRST,
    };
    let record = key.record();

    // On a fresh key: refused, nothing stored.
    assert!(node
        .write_pod_entry(alice.id, pod, PodStore::Records, &key.to_bytes(), b"")
        .await
        .is_err());
    assert_eq!(node.read_pod_record(alice.id, pod, &record).await?, None);

    // Over an existing value: refused, the previous value survives.
    node.write_pod_entry(alice.id, pod, PodStore::Records, &key.to_bytes(), b"claim")
        .await?;
    assert!(node
        .write_pod_entry(alice.id, pod, PodStore::Records, &key.to_bytes(), b"")
        .await
        .is_err());
    assert_eq!(
        node.read_pod_record(alice.id, pod, &record)
            .await?
            .as_deref(),
        Some(b"claim".as_ref())
    );

    node.shutdown().await?;
    Ok(())
}

/// A subscriber that stops reading holds up neither sync nor the device's
/// own calls: the laptop's PMS takes in more of the phone's records
/// than its unread subscription buffers, reads them all, and still writes
/// and lists, while the subscription reports the changes it dropped.
///
/// Six hundred records: past the 256-slot insert buffer and the 64 slots of
/// the RPC stream behind it, while the 256-slot live buffer does not fill,
/// the records sharing one payload. Each probe waits the whole budget: the
/// store stopping for good is what it catches, and a loaded runner holds
/// one call for seconds.
#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_that_stops_reading_holds_up_no_sync() -> Result<()> {
    const RECORDS: u16 = 600;
    let (phone, phone_pms, laptop, laptop_pms) = two_devices().await?;

    let mut unread = laptop_pms.changes().await?;
    let pod = |n: u16| {
        let mut bytes = [0x5e; 16];
        bytes[..2].copy_from_slice(&n.to_be_bytes());
        PodId::from_bytes(bytes)
    };
    for n in 0..RECORDS {
        phone_pms.record_pod(pod(n), Seq::FIRST).await?;
    }
    assert!(
        eventually(|| async {
            let held = tokio::time::timeout(TIMEOUT, laptop_pms.held_pods())
                .await
                .map_err(|_| anyhow::anyhow!("the laptop's store stopped answering"))??;
            Ok(held.len() >= usize::from(RECORDS))
        })
        .await?,
        "the laptop did not take in every record past its unread subscription"
    );
    tokio::time::timeout(TIMEOUT, laptop_pms.record_pod(FAMILY, Seq::FIRST))
        .await
        .map_err(|_| anyhow::anyhow!("a local write waited on the unread subscription"))??;

    // Read at last, the subscription still yields: the dropped changes are
    // reported, not lost silently.
    let reported = tokio::time::timeout(TIMEOUT, futures_lite::StreamExt::next(&mut unread)).await;
    assert!(
        matches!(reported, Ok(Some(Ok(())))),
        "the subscription went silent over the changes it dropped"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}
