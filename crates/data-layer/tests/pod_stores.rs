//! A pod's two stores as replica kinds of one hosted identity: created,
//! imported from their write tickets, forgotten at a departure into the
//! pod's tombstone, and held once per member identity on one node. The join
//! dialogue and the directory that carry a pod's tickets live in pdn-node;
//! here the tickets travel by hand, and the import each reaches is its
//! scenario's subject, as a granted data replica arrives by the import the
//! grant binder performs. No session on a pod's store is served, so nothing
//! here asserts what flows through one.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, IdentityNotProvisioned, NamespaceId, PodTickets, PrivateMetadataStore,
    ShareMode, SyncNode, UnknownPod,
};
use pdn_types::{PdnId, PodId};
use test_utils::{host_identity, ids, memory_node};

/// "Family", `ad58a3faa04cdc5576c8dc5823a347c6` in the specs' examples.
const FAMILY: PodId = PodId::from_bytes([
    0xad, 0x58, 0xa3, 0xfa, 0xa0, 0x4c, 0xdc, 0x55, 0x76, 0xc8, 0xdc, 0x58, 0x23, 0xa3, 0x47, 0xc6,
]);
/// "Wedding", `f942dfc21acd0218d48f61f714ddfff3` in the specs' examples.
const WEDDING: PodId = PodId::from_bytes([
    0xf9, 0x42, 0xdf, 0xc2, 0x1a, 0xcd, 0x02, 0x18, 0xd4, 0x8f, 0x61, 0xf7, 0x14, 0xdd, 0xff, 0xf3,
]);

async fn tickets(node: &SyncNode, identity: PdnId, pod: PodId) -> Result<PodTickets> {
    node.share_pod_tickets(identity, pod, AddrInfoOptions::Addresses)
        .await
}

fn namespaces(tickets: &PodTickets) -> (NamespaceId, NamespaceId) {
    (
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    )
}

fn is_unknown_pod(err: &anyhow::Error, pod: PodId) -> bool {
    err.downcast_ref::<UnknownPod>()
        .is_some_and(|unknown| unknown.pod == pod)
}

/// Creating a pod allocates two fresh replicas for the creating identity,
/// reached through the pod id and binding no data namespace, and a second
/// pod of the same identity two more.
#[tokio::test(flavor = "multi_thread")]
async fn creating_a_pod_allocates_two_dedicated_replicas() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::ALICE).await?;

    node.create_pod(ids::ALICE, FAMILY).await?;
    node.create_pod(ids::ALICE, WEDDING).await?;
    let (family_membership, family_records) =
        namespaces(&tickets(&node, ids::ALICE, FAMILY).await?);
    let (wedding_membership, wedding_records) =
        namespaces(&tickets(&node, ids::ALICE, WEDDING).await?);

    let all = [
        family_membership,
        family_records,
        wedding_membership,
        wedding_records,
    ];
    for (index, namespace) in all.iter().enumerate() {
        assert!(
            node.holds_replica(ids::ALICE, *namespace).await?,
            "a pod's store is not held"
        );
        assert!(
            !all.iter().skip(index + 1).any(|other| other == namespace),
            "two pods' stores share a replica"
        );
    }
    assert_eq!(
        node.data_namespace_of(ids::ALICE, ids::ALICE)?,
        None,
        "a pod bound a data namespace"
    );
    assert!(
        node.create_pod(ids::ALICE, FAMILY).await.is_err(),
        "a pod id the identity holds was created again"
    );

    node.shutdown().await?;
    Ok(())
}

/// An import of a pod's stores is refused, with nothing registered, when a
/// ticket names a namespace the identity already holds in another role: a
/// data store received under a grant, its directory, another pod's store,
/// or the pod's other store. The same tickets, honest, import afterwards,
/// and every replica the refusals named is still held in its own role.
#[allow(clippy::too_many_lines)] // one scenario: every role a refused ticket names, beside the honest import
#[tokio::test(flavor = "multi_thread")]
async fn a_store_ticket_held_in_another_role_is_refused() -> Result<()> {
    let bob_phone = memory_node().await?;
    let alice_phone = memory_node().await?;
    host_identity(&bob_phone, ids::BOB).await?;
    let alice_directory = host_identity(&alice_phone, ids::ALICE).await?;

    bob_phone.create_namespace(ids::BOB, ids::BOB).await?;
    let bob_data = bob_phone
        .share_ticket(
            ids::BOB,
            ids::BOB,
            ShareMode::Write,
            AddrInfoOptions::Addresses,
        )
        .await?;
    let _granted = alice_phone
        .import_namespace_granted(ids::ALICE, ids::BOB, bob_data.clone())
        .await?;
    alice_phone.create_pod(ids::ALICE, WEDDING).await?;
    let wedding = tickets(&alice_phone, ids::ALICE, WEDDING).await?;
    let directory_ticket = alice_directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    bob_phone.create_pod(ids::BOB, FAMILY).await?;
    let family = tickets(&bob_phone, ids::BOB, FAMILY).await?;

    let refused = [
        (
            "a data store received under a grant",
            PodTickets {
                membership: bob_data.clone(),
                records: family.records.clone(),
            },
        ),
        (
            "the identity's directory",
            PodTickets {
                membership: family.membership.clone(),
                records: directory_ticket,
            },
        ),
        (
            "another pod's store",
            PodTickets {
                membership: wedding.membership.clone(),
                records: family.records.clone(),
            },
        ),
        (
            "the pod's other store",
            PodTickets {
                membership: family.membership.clone(),
                records: family.membership.clone(),
            },
        ),
    ];
    for (role, tickets_in_role) in refused {
        assert!(
            alice_phone
                .import_pod(ids::ALICE, FAMILY, tickets_in_role)
                .await
                .is_err(),
            "a ticket naming {role} was imported as a store of the pod"
        );
        let after = tickets(&alice_phone, ids::ALICE, FAMILY).await;
        assert!(
            after.is_err_and(|err| is_unknown_pod(&err, FAMILY)),
            "a refused import naming {role} registered the pod"
        );
    }

    assert_eq!(
        alice_phone.data_namespace_of(ids::ALICE, ids::BOB)?,
        Some(bob_data.capability.id()),
        "the granted data store lost its binding"
    );
    assert!(
        alice_phone
            .holds_replica(ids::ALICE, alice_directory.namespace())
            .await?,
        "the directory stopped being held"
    );
    assert_eq!(
        namespaces(&tickets(&alice_phone, ids::ALICE, WEDDING).await?),
        namespaces(&wedding),
        "the other pod's stores changed"
    );
    alice_phone
        .import_pod(ids::ALICE, FAMILY, family.clone())
        .await?;
    assert_eq!(
        namespaces(&tickets(&alice_phone, ids::ALICE, FAMILY).await?),
        namespaces(&family),
        "the honest tickets did not import"
    );

    alice_phone.shutdown().await?;
    bob_phone.shutdown().await?;
    Ok(())
}

/// A data import handed a ticket naming a pod's store is refused, as a
/// device of the issuer and as a grantee, and so is a directory import; the
/// membership store is still refused once it is the pod's tombstone.
#[tokio::test(flavor = "multi_thread")]
async fn a_data_import_refuses_a_pods_store() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::ALICE).await?;
    node.create_pod(ids::ALICE, FAMILY).await?;
    let family = tickets(&node, ids::ALICE, FAMILY).await?;

    assert!(
        node.import_namespace(ids::ALICE, ids::BOB, family.records.clone())
            .await
            .is_err(),
        "a device data import took the record store"
    );
    assert!(
        node.import_namespace_granted(ids::ALICE, ids::BOB, family.membership.clone())
            .await
            .is_err(),
        "a grantee data import took the membership store"
    );
    assert!(
        PrivateMetadataStore::import(&node, ids::ALICE, family.records.clone())
            .await
            .is_err(),
        "a directory import took the record store"
    );
    assert_eq!(node.data_namespace_of(ids::ALICE, ids::BOB)?, None);
    assert_eq!(
        namespaces(&tickets(&node, ids::ALICE, FAMILY).await?),
        namespaces(&family),
        "a refused import changed the pod's stores"
    );

    node.forget_pod(ids::ALICE, FAMILY).await?;
    assert!(
        node.import_namespace(ids::ALICE, ids::BOB, family.membership.clone())
            .await
            .is_err(),
        "a data import took the tombstone"
    );

    node.shutdown().await?;
    Ok(())
}

/// Forgetting a pod at a departure drops its record store and keeps its
/// membership store as the tombstone; operations addressed to the pod then
/// fail with the unknown-pod error, a second forget finishes quietly, and
/// the identity's other pod is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn forgetting_a_pod_keeps_the_membership_store_as_its_tombstone() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::ALICE).await?;
    node.create_pod(ids::ALICE, FAMILY).await?;
    node.create_pod(ids::ALICE, WEDDING).await?;
    let (membership, records) = namespaces(&tickets(&node, ids::ALICE, FAMILY).await?);
    let wedding = namespaces(&tickets(&node, ids::ALICE, WEDDING).await?);

    node.forget_pod(ids::ALICE, FAMILY).await?;
    assert!(
        !node.holds_replica(ids::ALICE, records).await?,
        "the record store outlived the departure"
    );
    assert!(
        node.holds_replica(ids::ALICE, membership).await?,
        "the membership store did not stay as the tombstone"
    );
    let shared = tickets(&node, ids::ALICE, FAMILY).await;
    assert!(
        shared.is_err_and(|err| is_unknown_pod(&err, FAMILY)),
        "the forgotten pod was not unknown"
    );
    node.forget_pod(ids::ALICE, FAMILY).await?;
    assert!(
        node.holds_replica(ids::ALICE, membership).await?,
        "a second forget dropped the tombstone"
    );

    assert_eq!(
        namespaces(&tickets(&node, ids::ALICE, WEDDING).await?),
        wedding,
        "the other pod changed"
    );
    for namespace in [wedding.0, wedding.1] {
        assert!(node.holds_replica(ids::ALICE, namespace).await?);
    }

    // The unknown-pod error names the pod alone: an identity with no half
    // here fails otherwise.
    let never = PodId::from_bytes([0x68; 16]);
    let never_held = node.forget_pod(ids::ALICE, never).await;
    assert!(never_held.is_err_and(|err| is_unknown_pod(&err, never)));
    let unhosted = node.forget_pod(ids::BOB, WEDDING).await;
    assert!(
        unhosted.is_err_and(|err| err.downcast_ref::<IdentityNotProvisioned>().is_some()
            && err.downcast_ref::<UnknownPod>().is_none())
    );

    node.shutdown().await?;
    Ok(())
}

/// Two members hosted on one node each hold the pod's two stores in a
/// replica of their own, and one of them forgetting the pod leaves the
/// other's copy held.
#[tokio::test(flavor = "multi_thread")]
async fn one_member_forgetting_spares_the_co_located_other() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::BOB).await?;
    host_identity(&node, ids::DAVE).await?;
    node.create_pod(ids::BOB, FAMILY).await?;
    let family = tickets(&node, ids::BOB, FAMILY).await?;
    node.import_pod(ids::DAVE, FAMILY, family.clone()).await?;
    let (membership, records) = namespaces(&family);
    for identity in [ids::BOB, ids::DAVE] {
        for namespace in [membership, records] {
            assert!(
                node.holds_replica(identity, namespace).await?,
                "a member on the node holds no replica of its own"
            );
        }
    }

    node.forget_pod(ids::BOB, FAMILY).await?;
    let bob = tickets(&node, ids::BOB, FAMILY).await;
    assert!(bob.is_err_and(|err| is_unknown_pod(&err, FAMILY)));
    assert!(!node.holds_replica(ids::BOB, records).await?);
    assert_eq!(
        namespaces(&tickets(&node, ids::DAVE, FAMILY).await?),
        (membership, records),
        "the co-located member lost the pod"
    );
    for namespace in [membership, records] {
        assert!(
            node.holds_replica(ids::DAVE, namespace).await?,
            "the co-located member's replica went with the other's forget"
        );
    }

    node.shutdown().await?;
    Ok(())
}

/// A member that departed and joins again imports the pod's tickets onto
/// its tombstone and holds the pod again; the same tickets imported while
/// held bind nothing. Paired denial: tickets naming other stores for a held
/// pod are refused, and the pod stays on its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_departed_member_holds_the_pod_again_on_its_tombstone() -> Result<()> {
    let alice_phone = memory_node().await?;
    let carol_phone = memory_node().await?;
    host_identity(&alice_phone, ids::ALICE).await?;
    host_identity(&carol_phone, ids::CAROL).await?;
    alice_phone.create_pod(ids::ALICE, FAMILY).await?;
    alice_phone.create_pod(ids::ALICE, WEDDING).await?;
    let family = tickets(&alice_phone, ids::ALICE, FAMILY).await?;
    let wedding = tickets(&alice_phone, ids::ALICE, WEDDING).await?;

    carol_phone
        .import_pod(ids::CAROL, FAMILY, family.clone())
        .await?;
    carol_phone
        .import_pod(ids::CAROL, FAMILY, family.clone())
        .await?;
    // Denied: another pod's stores, whole or the record store alone.
    for other in [
        wedding.clone(),
        PodTickets {
            membership: family.membership.clone(),
            records: wedding.records.clone(),
        },
    ] {
        assert!(
            carol_phone
                .import_pod(ids::CAROL, FAMILY, other)
                .await
                .is_err(),
            "a held pod was rebound onto other stores"
        );
    }
    assert_eq!(
        namespaces(&tickets(&carol_phone, ids::CAROL, FAMILY).await?),
        namespaces(&family)
    );

    carol_phone.forget_pod(ids::CAROL, FAMILY).await?;
    let departed = tickets(&carol_phone, ids::CAROL, FAMILY).await;
    assert!(departed.is_err_and(|err| is_unknown_pod(&err, FAMILY)));
    carol_phone
        .import_pod(ids::CAROL, FAMILY, family.clone())
        .await?;
    assert_eq!(
        namespaces(&tickets(&carol_phone, ids::CAROL, FAMILY).await?),
        namespaces(&family),
        "the rejoined member does not hold the pod"
    );
    assert!(
        carol_phone
            .holds_replica(ids::CAROL, family.records.capability.id())
            .await?
    );

    carol_phone.shutdown().await?;
    alice_phone.shutdown().await?;
    Ok(())
}

/// A pod whose record store fails to create leaves nothing behind: no pod
/// registered, no store on the reconcile pass and no replica in the store,
/// and the same id creates afterwards.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn a_pod_whose_record_store_fails_to_create_leaves_nothing() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::ALICE).await?;
    let tracked_before = node.tracked_doc_count(ids::ALICE)?;
    let held_before = node.held_replica_count(ids::ALICE).await?;

    node.fail_next_pod_records_create_for_test();
    assert!(node.create_pod(ids::ALICE, FAMILY).await.is_err());
    let after_failure = tickets(&node, ids::ALICE, FAMILY).await;
    assert!(
        after_failure.is_err_and(|err| is_unknown_pod(&err, FAMILY)),
        "a half-created pod was registered"
    );
    assert_eq!(
        node.tracked_doc_count(ids::ALICE)?,
        tracked_before,
        "a half-created pod's store went on the reconcile pass"
    );
    assert_eq!(
        node.held_replica_count(ids::ALICE).await?,
        held_before,
        "a half-created pod's membership store stayed in the store"
    );

    node.create_pod(ids::ALICE, FAMILY).await?;
    assert_eq!(node.tracked_doc_count(ids::ALICE)?, tracked_before + 2);

    node.shutdown().await?;
    Ok(())
}
