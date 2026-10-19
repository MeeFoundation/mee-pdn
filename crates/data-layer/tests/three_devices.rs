//! Three devices, two identities, a partial overlap: a device that
//! replicated a store is a full peer, and what it holds is sufficient to
//! bring up the next device.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, DocTicket, IdentityNotProvisioned, PrivateMetadataStore, Seq, SyncNode,
};
use pdn_types::{PdnId, PodId};
use test_utils::{eventually, host_identity, ids, join_identity, memory_node, wait_devices};

const WORK_POD: PodId = PodId::from_bytes([0xa2; 16]);
const LEISURE_POD: PodId = PodId::from_bytes([0xa3; 16]);
const FAMILY: PodId = PodId::from_bytes([0xfa; 16]);

/// Bring one identity up on `phone` with its fixture: a PMS with the phone
/// registered and a record of `pod`. Returns the phone-side PMS and its
/// write ticket.
async fn provision_with_fixtures(
    phone: &SyncNode,
    identity: PdnId,
    pod: PodId,
) -> Result<(PrivateMetadataStore, DocTicket)> {
    let pms = host_identity(phone, identity).await?;
    pms.record_pod(pod, Seq::FIRST).await?;
    let ticket = pms.share_ticket(AddrInfoOptions::RelayAndAddresses).await?;
    Ok((pms, ticket))
}

/// Bring the identity behind `ticket` up on `node`, as device linking does
/// at the store level: bring up the identity's stores, import the
/// PMS and join the device set.
async fn join_from(
    node: &SyncNode,
    identity: PdnId,
    ticket: DocTicket,
) -> Result<PrivateMetadataStore> {
    let pms = join_identity(node, identity, ticket).await?;
    pms.add_device(node.node_id()).await?;
    Ok(pms)
}

/// Whether `pms` comes to hold `pod`.
async fn wait_holds(pms: &PrivateMetadataStore, pod: PodId) -> Result<bool> {
    eventually(|| async { Ok(pms.pod_record(pod).await? == Some((Seq::FIRST, true))) }).await
}

/// The tablet joins the work identity from the ticket the laptop minted,
/// not the first device: state authored on the phone reaches it
/// transitively, a live update crosses the three-device swarm, the device
/// sets end up asymmetric (work: three, leisure: two), and the tablet knows
/// nothing of the leisure identity.
#[tokio::test(flavor = "multi_thread")]
async fn three_devices_two_identities() -> Result<()> {
    let phone = memory_node().await?;
    let laptop = memory_node().await?;
    let tablet = memory_node().await?;
    let phone_id = phone.node_id();
    let laptop_id = laptop.node_id();
    let tablet_id = tablet.node_id();

    // Phone brings up both identities, each with a pod record.
    let (work_phone_pms, work_ticket) =
        provision_with_fixtures(&phone, ids::ALICE_AT_WORK, WORK_POD).await?;
    let (_leisure_phone_pms, leisure_ticket) =
        provision_with_fixtures(&phone, ids::ALICE_AT_LEISURE, LEISURE_POD).await?;

    // Laptop joins both identities from phone's tickets.
    let work_laptop_pms = join_from(&laptop, ids::ALICE_AT_WORK, work_ticket).await?;
    let leisure_laptop_pms = join_from(&laptop, ids::ALICE_AT_LEISURE, leisure_ticket).await?;
    assert!(
        wait_holds(&work_laptop_pms, WORK_POD).await?,
        "the work record did not reach laptop"
    );

    // Tablet joins work only, from the PMS ticket the LAPTOP mints.
    let tablet_ticket = work_laptop_pms
        .share_ticket(AddrInfoOptions::RelayAndAddresses)
        .await?;
    let work_tablet_pms = join_from(&tablet, ids::ALICE_AT_WORK, tablet_ticket).await?;

    // Transitive catch-up: state authored on phone reaches the tablet
    // through the store it obtained via the laptop.
    assert!(
        wait_holds(&work_tablet_pms, WORK_POD).await?,
        "the work record did not reach the tablet"
    );

    // Live through the three-device swarm: a fresh record on phone.
    work_phone_pms.record_pod(FAMILY, Seq::FIRST).await?;
    assert!(
        wait_holds(&work_tablet_pms, FAMILY).await?,
        "a live work update did not reach the tablet"
    );

    // The work device set converges to all three — on the first device too.
    let all = [phone_id, laptop_id, tablet_id];
    assert!(
        wait_devices(&work_tablet_pms, &all).await?,
        "the tablet's work device set is incomplete"
    );
    assert!(
        wait_devices(&work_phone_pms, &all).await?,
        "the phone's work device set is incomplete"
    );

    // The leisure device set stays at two: the tablet is not in it.
    assert!(
        wait_devices(&leisure_laptop_pms, &[phone_id, laptop_id]).await?,
        "the leisure device set did not converge"
    );
    assert!(
        !leisure_laptop_pms
            .list_devices()
            .await?
            .contains(&tablet_id),
        "the tablet leaked into the leisure device set"
    );

    // And the tablet knows nothing of leisure: the identity is not hosted
    // there, and its record never reached the work PMS.
    let err = tablet
        .pod_stores(ids::ALICE_AT_LEISURE, LEISURE_POD)
        .unwrap_err();
    assert!(err.downcast_ref::<IdentityNotProvisioned>().is_some());
    assert_eq!(work_tablet_pms.pod_record(LEISURE_POD).await?, None);

    phone.shutdown().await?;
    laptop.shutdown().await?;
    tablet.shutdown().await?;
    Ok(())
}
