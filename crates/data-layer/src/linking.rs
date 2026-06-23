//! Device linking: bring a new device up from a single seed.
//!
//! [`link_device`] takes the private-metadata-store ticket — the seed, as
//! carried in a QR — imports that directory, registers the new device in it,
//! then discovers and imports the identity's other stores through it. The
//! private metadata store necessarily comes first: every other store is
//! found via a ticket stored inside it.
//!
//! The seed is a bearer ticket; identity-bound linking is future work.
//! Importing data namespaces during linking is deferred (ADR-0009).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use pdn_store::DocTicket;
use pdn_types::PdnId;

use crate::connections::ConnectionsStore;
use crate::node::SyncNode;
use crate::private_metadata::PrivateMetadataStore;

/// Directory key under which the connections-store ticket lives.
const CONNECTIONS_TICKET: &str = "connections";

/// The stores a newly linked device brings up from the seed.
///
/// Non-exhaustive: it grows as more stores are discovered through the
/// directory (data namespaces are deferred — ADR-0009).
#[derive(Debug)]
#[non_exhaustive]
pub struct LinkedStores {
    /// The bootstrap directory itself.
    pub private_metadata: PrivateMetadataStore,
    /// The connections store, discovered through the directory.
    pub connections: ConnectionsStore,
}

/// Link `node` as a device of `identity`, given only `seed` — the
/// private-metadata-store ticket.
///
/// Imports the directory, registers this device in it
/// ([`PrivateMetadataStore::add_device`] with [`SyncNode::node_id`]), then
/// waits up to `timeout` for the connections-store ticket to sync in and
/// imports the connections store. Returns once the stores are imported;
/// replicating their contents is ongoing sync the caller observes. A
/// stalled directory surfaces as a timeout error, not a hang.
pub async fn link_device(
    node: &mut SyncNode,
    identity: PdnId,
    seed: DocTicket,
    timeout: Duration,
) -> Result<LinkedStores> {
    // Directory first — everything else is discovered through it.
    let private_metadata = PrivateMetadataStore::import(node, identity, seed).await?;

    // Discover the connections-store ticket from the directory, then import it.
    let connections_ticket = wait_for_ticket(&private_metadata, CONNECTIONS_TICKET, timeout)
        .await?
        .context("connections ticket did not sync into the private metadata store")?;
    let connections = ConnectionsStore::import(node, identity, connections_ticket).await?;

    // Join the device set so the identity's other devices see this one. Done
    // after catch-up so the local write doesn't race the import's first sync.
    private_metadata.add_device(node.node_id()).await?;

    Ok(LinkedStores {
        private_metadata,
        connections,
    })
}

/// Poll the directory for the ticket of `kind` until it syncs in, or time out.
async fn wait_for_ticket(
    store: &PrivateMetadataStore,
    kind: &str,
    timeout: Duration,
) -> Result<Option<DocTicket>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(ticket) = store.get_ticket(kind).await? {
            return Ok(Some(ticket));
        }
        if Instant::now() > deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
