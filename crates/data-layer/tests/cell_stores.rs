//! A cell's two stores as replica kinds of one hosted identity: created,
//! imported from their write tickets, forgotten at a departure into the
//! cell's tombstone, and held once per member identity on one node. The join
//! dialogue and the directory that carry a cell's tickets live in pdn-node;
//! here the tickets travel by hand, and the import each reaches is its
//! scenario's subject, as a granted data replica arrives by the import the
//! grant binder performs. No session on a cell's store is served, so nothing
//! here asserts what flows through one.

use anyhow::Result;
use data_layer::{
    AddrInfoOptions, CellTickets, IdentityNotProvisioned, NamespaceId, PrivateMetadataStore,
    ShareMode, SyncNode, UnknownCell,
};
use pdn_types::{CellId, PdnId};
use test_utils::{host_identity, ids, memory_node};

/// "Family", `9cbcbe4da7cc35a44360d64e45621957` in the specs' examples.
const FAMILY: CellId = CellId::from_bytes([
    0x9c, 0xbc, 0xbe, 0x4d, 0xa7, 0xcc, 0x35, 0xa4, 0x43, 0x60, 0xd6, 0x4e, 0x45, 0x62, 0x19, 0x57,
]);
/// "Wedding", `f942dfc21acd0218d48f61f714ddfff3` in the specs' examples.
const WEDDING: CellId = CellId::from_bytes([
    0xf9, 0x42, 0xdf, 0xc2, 0x1a, 0xcd, 0x02, 0x18, 0xd4, 0x8f, 0x61, 0xf7, 0x14, 0xdd, 0xff, 0xf3,
]);

async fn tickets(node: &SyncNode, identity: PdnId, cell: CellId) -> Result<CellTickets> {
    node.share_cell_tickets(identity, cell, AddrInfoOptions::Addresses)
        .await
}

fn namespaces(tickets: &CellTickets) -> (NamespaceId, NamespaceId) {
    (
        tickets.membership.capability.id(),
        tickets.records.capability.id(),
    )
}

fn is_unknown_cell(err: &anyhow::Error, cell: CellId) -> bool {
    err.downcast_ref::<UnknownCell>()
        .is_some_and(|unknown| unknown.cell == cell)
}

/// Creating a cell allocates two fresh replicas for the creating identity,
/// reached through the cell id and binding no data namespace, and a second
/// cell of the same identity two more.
#[tokio::test(flavor = "multi_thread")]
async fn creating_a_cell_allocates_two_dedicated_replicas() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::ALICE).await?;

    node.create_cell(ids::ALICE, FAMILY).await?;
    node.create_cell(ids::ALICE, WEDDING).await?;
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
            "a cell's store is not held"
        );
        assert!(
            !all.iter().skip(index + 1).any(|other| other == namespace),
            "two cells' stores share a replica"
        );
    }
    assert_eq!(
        node.data_namespace_of(ids::ALICE, ids::ALICE)?,
        None,
        "a cell bound a data namespace"
    );
    assert!(
        node.create_cell(ids::ALICE, FAMILY).await.is_err(),
        "a cell id the identity holds was created again"
    );

    node.shutdown().await?;
    Ok(())
}

/// An import of a cell's stores is refused, with nothing registered, when a
/// ticket names a namespace the identity already holds in another role: a
/// data store received under a grant, its directory, another cell's store,
/// or the cell's other store. The same tickets, honest, import afterwards,
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
    alice_phone.create_cell(ids::ALICE, WEDDING).await?;
    let wedding = tickets(&alice_phone, ids::ALICE, WEDDING).await?;
    let directory_ticket = alice_directory
        .share_ticket(ShareMode::Write, AddrInfoOptions::Addresses)
        .await?;
    bob_phone.create_cell(ids::BOB, FAMILY).await?;
    let family = tickets(&bob_phone, ids::BOB, FAMILY).await?;

    let refused = [
        (
            "a data store received under a grant",
            CellTickets {
                membership: bob_data.clone(),
                records: family.records.clone(),
            },
        ),
        (
            "the identity's directory",
            CellTickets {
                membership: family.membership.clone(),
                records: directory_ticket,
            },
        ),
        (
            "another cell's store",
            CellTickets {
                membership: wedding.membership.clone(),
                records: family.records.clone(),
            },
        ),
        (
            "the cell's other store",
            CellTickets {
                membership: family.membership.clone(),
                records: family.membership.clone(),
            },
        ),
    ];
    for (role, tickets_in_role) in refused {
        assert!(
            alice_phone
                .import_cell(ids::ALICE, FAMILY, tickets_in_role)
                .await
                .is_err(),
            "a ticket naming {role} was imported as a store of the cell"
        );
        let after = tickets(&alice_phone, ids::ALICE, FAMILY).await;
        assert!(
            after.is_err_and(|err| is_unknown_cell(&err, FAMILY)),
            "a refused import naming {role} registered the cell"
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
        "the other cell's stores changed"
    );
    alice_phone
        .import_cell(ids::ALICE, FAMILY, family.clone())
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

/// A data import handed a ticket naming a cell's store is refused, as a
/// device of the issuer and as a grantee, and so is a directory import; the
/// membership store is still refused once it is the cell's tombstone.
#[tokio::test(flavor = "multi_thread")]
async fn a_data_import_refuses_a_cells_store() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::ALICE).await?;
    node.create_cell(ids::ALICE, FAMILY).await?;
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
        "a refused import changed the cell's stores"
    );

    node.forget_cell(ids::ALICE, FAMILY).await?;
    assert!(
        node.import_namespace(ids::ALICE, ids::BOB, family.membership.clone())
            .await
            .is_err(),
        "a data import took the tombstone"
    );

    node.shutdown().await?;
    Ok(())
}

/// Forgetting a cell at a departure drops its record store and keeps its
/// membership store as the tombstone; operations addressed to the cell then
/// fail with the unknown-cell error, a second forget finishes quietly, and
/// the identity's other cell is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn forgetting_a_cell_keeps_the_membership_store_as_its_tombstone() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::ALICE).await?;
    node.create_cell(ids::ALICE, FAMILY).await?;
    node.create_cell(ids::ALICE, WEDDING).await?;
    let (membership, records) = namespaces(&tickets(&node, ids::ALICE, FAMILY).await?);
    let wedding = namespaces(&tickets(&node, ids::ALICE, WEDDING).await?);

    node.forget_cell(ids::ALICE, FAMILY).await?;
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
        shared.is_err_and(|err| is_unknown_cell(&err, FAMILY)),
        "the forgotten cell was not unknown"
    );
    node.forget_cell(ids::ALICE, FAMILY).await?;
    assert!(
        node.holds_replica(ids::ALICE, membership).await?,
        "a second forget dropped the tombstone"
    );

    assert_eq!(
        namespaces(&tickets(&node, ids::ALICE, WEDDING).await?),
        wedding,
        "the other cell changed"
    );
    for namespace in [wedding.0, wedding.1] {
        assert!(node.holds_replica(ids::ALICE, namespace).await?);
    }

    // The unknown-cell error names the cell alone: an identity with no half
    // here fails otherwise.
    let never = CellId::from_bytes([0x68; 16]);
    let never_held = node.forget_cell(ids::ALICE, never).await;
    assert!(never_held.is_err_and(|err| is_unknown_cell(&err, never)));
    let unhosted = node.forget_cell(ids::BOB, WEDDING).await;
    assert!(
        unhosted.is_err_and(|err| err.downcast_ref::<IdentityNotProvisioned>().is_some()
            && err.downcast_ref::<UnknownCell>().is_none())
    );

    node.shutdown().await?;
    Ok(())
}

/// Two members hosted on one node each hold the cell's two stores in a
/// replica of their own, and one of them forgetting the cell leaves the
/// other's copy held.
#[tokio::test(flavor = "multi_thread")]
async fn one_member_forgetting_spares_the_co_located_other() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::BOB).await?;
    host_identity(&node, ids::DAVE).await?;
    node.create_cell(ids::BOB, FAMILY).await?;
    let family = tickets(&node, ids::BOB, FAMILY).await?;
    node.import_cell(ids::DAVE, FAMILY, family.clone()).await?;
    let (membership, records) = namespaces(&family);
    for identity in [ids::BOB, ids::DAVE] {
        for namespace in [membership, records] {
            assert!(
                node.holds_replica(identity, namespace).await?,
                "a member on the node holds no replica of its own"
            );
        }
    }

    node.forget_cell(ids::BOB, FAMILY).await?;
    let bob = tickets(&node, ids::BOB, FAMILY).await;
    assert!(bob.is_err_and(|err| is_unknown_cell(&err, FAMILY)));
    assert!(!node.holds_replica(ids::BOB, records).await?);
    assert_eq!(
        namespaces(&tickets(&node, ids::DAVE, FAMILY).await?),
        (membership, records),
        "the co-located member lost the cell"
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

/// A member that departed and joins again imports the cell's tickets onto
/// its tombstone and holds the cell again; the same tickets imported while
/// held bind nothing. Paired denial: tickets naming other stores for a held
/// cell are refused, and the cell stays on its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_departed_member_holds_the_cell_again_on_its_tombstone() -> Result<()> {
    let alice_phone = memory_node().await?;
    let carol_phone = memory_node().await?;
    host_identity(&alice_phone, ids::ALICE).await?;
    host_identity(&carol_phone, ids::CAROL).await?;
    alice_phone.create_cell(ids::ALICE, FAMILY).await?;
    alice_phone.create_cell(ids::ALICE, WEDDING).await?;
    let family = tickets(&alice_phone, ids::ALICE, FAMILY).await?;
    let wedding = tickets(&alice_phone, ids::ALICE, WEDDING).await?;

    carol_phone
        .import_cell(ids::CAROL, FAMILY, family.clone())
        .await?;
    carol_phone
        .import_cell(ids::CAROL, FAMILY, family.clone())
        .await?;
    // Denied: another cell's stores, whole or the record store alone.
    for other in [
        wedding.clone(),
        CellTickets {
            membership: family.membership.clone(),
            records: wedding.records.clone(),
        },
    ] {
        assert!(
            carol_phone
                .import_cell(ids::CAROL, FAMILY, other)
                .await
                .is_err(),
            "a held cell was rebound onto other stores"
        );
    }
    assert_eq!(
        namespaces(&tickets(&carol_phone, ids::CAROL, FAMILY).await?),
        namespaces(&family)
    );

    carol_phone.forget_cell(ids::CAROL, FAMILY).await?;
    let departed = tickets(&carol_phone, ids::CAROL, FAMILY).await;
    assert!(departed.is_err_and(|err| is_unknown_cell(&err, FAMILY)));
    carol_phone
        .import_cell(ids::CAROL, FAMILY, family.clone())
        .await?;
    assert_eq!(
        namespaces(&tickets(&carol_phone, ids::CAROL, FAMILY).await?),
        namespaces(&family),
        "the rejoined member does not hold the cell"
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

/// A cell whose record store fails to create leaves nothing behind: no cell
/// registered, no store on the reconcile pass and no replica in the store,
/// and the same id creates afterwards.
#[cfg(feature = "test-util")]
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_whose_record_store_fails_to_create_leaves_nothing() -> Result<()> {
    let node = memory_node().await?;
    host_identity(&node, ids::ALICE).await?;
    let tracked_before = node.tracked_doc_count(ids::ALICE)?;
    let held_before = node.held_replica_count(ids::ALICE).await?;

    node.fail_next_cell_records_create_for_test();
    assert!(node.create_cell(ids::ALICE, FAMILY).await.is_err());
    let after_failure = tickets(&node, ids::ALICE, FAMILY).await;
    assert!(
        after_failure.is_err_and(|err| is_unknown_cell(&err, FAMILY)),
        "a half-created cell was registered"
    );
    assert_eq!(
        node.tracked_doc_count(ids::ALICE)?,
        tracked_before,
        "a half-created cell's store went on the reconcile pass"
    );
    assert_eq!(
        node.held_replica_count(ids::ALICE).await?,
        held_before,
        "a half-created cell's membership store stayed in the store"
    );

    node.create_cell(ids::ALICE, FAMILY).await?;
    assert_eq!(node.tracked_doc_count(ids::ALICE)?, tracked_before + 2);

    node.shutdown().await?;
    Ok(())
}
