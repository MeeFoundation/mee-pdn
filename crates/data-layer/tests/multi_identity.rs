//! One pair of devices hosts two identities side by side — Alice-at-work
//! and Alice-at-leisure, each a `PdnId` with its own PMS. The phone brings
//! up both; the laptop joins each by a separate import of that identity's
//! PMS ticket.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, DocTicket, IdentityNotProvisioned, PrivateMetadataStore, Seq, ShareMode,
    SyncNode,
};
use pdn_types::{PdnId, PodId};
use test_utils::{eventually, host_identity, ids, join_identity, memory_node, wait_devices};

/// Bring one identity up on `node` with its fixtures: a PMS with the node
/// registered, a record of `pod`, and `ticket` published under `KIND`.
/// Returns the PMS and its write ticket.
async fn provision_identity_with_records(
    node: &SyncNode,
    identity: PdnId,
    pod: PodId,
    ticket: &DocTicket,
) -> Result<(PrivateMetadataStore, DocTicket)> {
    let pms = host_identity(node, identity).await?;
    pms.record_pod(pod, Seq::FIRST).await?;
    pms.put_ticket(KIND, ticket).await?;
    let write = pms
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    Ok((pms, write))
}

/// Ticket kinds, pod records: the same kind in both identities' PMSs with
/// a different ticket in each — mixed-up stores would surface as the
/// wrong ticket.
const KIND: &str = "affiliation";
const WORK_POD: PodId = PodId::from_bytes([0xa2; 16]);
const LEISURE_POD: PodId = PodId::from_bytes([0xa3; 16]);

/// Whether `pms` comes to read `want` under [`KIND`].
async fn wait_kind_is(pms: &PrivateMetadataStore, want: &DocTicket) -> Result<bool> {
    eventually(|| async {
        Ok(pms
            .get_ticket(KIND)
            .await?
            .is_some_and(|got| got.to_string() == want.to_string()))
    })
    .await
}

/// A ticket of its own replica, so each identity's fixture differs.
async fn fresh_ticket(node: &SyncNode, identity: PdnId) -> Result<DocTicket> {
    PrivateMetadataStore::create(node, identity)
        .await?
        .share_ticket(ShareMode::Read, AddrInfoOptions::RelayAndAddresses)
        .await
}

/// Both identities' PMSs replicate to the laptop, each by its own import;
/// joining the first brings nothing of the second, the two stay isolated,
/// and the first keeps operating after the second joins.
#[tokio::test(flavor = "multi_thread")]
async fn multi_identity_two_devices() -> Result<()> {
    // Phone hosts both identities side by side.
    let phone = memory_node().await?;
    let laptop = memory_node().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();
    phone.provision_identity(ids::ALICE_AT_WORK).await?;
    phone.provision_identity(ids::ALICE_AT_LEISURE).await?;
    let work_fixture = fresh_ticket(&phone, ids::ALICE_AT_WORK).await?;
    let leisure_fixture = fresh_ticket(&phone, ids::ALICE_AT_LEISURE).await?;
    let (work_phone_pms, work_ticket) =
        provision_identity_with_records(&phone, ids::ALICE_AT_WORK, WORK_POD, &work_fixture)
            .await?;
    let (_leisure_phone_pms, leisure_ticket) = provision_identity_with_records(
        &phone,
        ids::ALICE_AT_LEISURE,
        LEISURE_POD,
        &leisure_fixture,
    )
    .await?;

    // Join the laptop into the work identity only.
    let laptop_work_pms = join_identity(&laptop, ids::ALICE_AT_WORK, work_ticket).await?;
    laptop_work_pms.add_device(laptop.node_id()).await?;
    assert!(
        wait_kind_is(&laptop_work_pms, &work_fixture).await?,
        "work records did not replicate to laptop"
    );

    // Isolation: nothing of the leisure identity arrived through that act —
    // its pod is not in the work PMS, and the identity is unknown here
    // (specifically unknown, not just any error).
    assert_eq!(laptop_work_pms.pod_record(LEISURE_POD).await?, None);
    let err = laptop
        .pod_stores(ids::ALICE_AT_LEISURE, LEISURE_POD)
        .unwrap_err();
    assert!(err.downcast_ref::<IdentityNotProvisioned>().is_some());

    // Second identity joins the already-joined node by its own act.
    let laptop_leisure_pms = join_identity(&laptop, ids::ALICE_AT_LEISURE, leisure_ticket).await?;
    laptop_leisure_pms.add_device(laptop.node_id()).await?;
    assert!(
        wait_kind_is(&laptop_leisure_pms, &leisure_fixture).await?,
        "leisure records did not replicate to laptop"
    );
    assert_eq!(laptop_leisure_pms.pod_record(WORK_POD).await?, None);
    assert_eq!(
        laptop_leisure_pms.pod_record(LEISURE_POD).await?,
        Some((Seq::FIRST, true))
    );
    assert!(
        wait_devices(&laptop_leisure_pms, &[phone_id, laptop_id]).await?,
        "leisure device set did not converge on laptop"
    );

    // The first identity keeps operating after the second joined: a fresh
    // pod record and a fresh ticket (an LWW overwrite of the same kind)
    // both still reach the laptop.
    work_phone_pms.record_pod(FAMILY, Seq::FIRST).await?;
    assert!(
        eventually(|| async {
            Ok(laptop_work_pms.pod_record(FAMILY).await? == Some((Seq::FIRST, true)))
        })
        .await?,
        "work records stopped replicating after the second identity joined"
    );
    let rewritten = fresh_ticket(&phone, ids::ALICE_AT_WORK).await?;
    work_phone_pms.put_ticket(KIND, &rewritten).await?;
    assert!(
        wait_kind_is(&laptop_work_pms, &rewritten).await?,
        "work tickets stopped replicating after the second identity joined"
    );
    assert!(
        wait_kind_is(&laptop_leisure_pms, &leisure_fixture).await?,
        "the work rewrite reached the leisure PMS"
    );

    phone.shutdown().await?;
    laptop.shutdown().await?;
    Ok(())
}

const FAMILY: PodId = PodId::from_bytes([0xfa; 16]);
