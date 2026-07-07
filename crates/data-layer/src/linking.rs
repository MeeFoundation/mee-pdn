//! Device linking: bring an identity up on its first device, and every
//! further device up from a single seed.
//!
//! [`provision_identity`] is the first-device half: create the connections
//! store, publish its ticket into a fresh private-metadata directory,
//! register the device. [`link_device`] is the every-further-device half: it
//! takes the private-metadata-store ticket — the seed, as carried in a QR —
//! imports that directory, registers the new device in it, then discovers
//! and imports the identity's other stores through it. The private metadata
//! store necessarily comes first: every other store is found via a ticket
//! stored inside it.
//!
//! Linking is per identity and repeatable: a node hosting several identities
//! runs it once per identity, each time with that identity's seed, and one
//! linking act imports nothing of any other identity. Adding an identity to
//! a device is always this explicit act — nothing cascades or propagates
//! automatically.
//!
//! The seed is a bearer ticket; identity-bound linking is future work.
//! Importing data namespaces during linking is deferred (ADR-0009).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    DocTicket,
};

use crate::connections::ConnectionsStore;
use crate::node::SyncNode;
use crate::private_metadata::PrivateMetadataStore;

/// Directory key under which the connections-store ticket lives.
const CONNECTIONS_TICKET: &str = "connections";

/// An identity's store set on this device, as assembled by
/// [`provision_identity`] (first device) or [`link_device`] (every further
/// device).
///
/// Non-exhaustive: it grows as more stores join the set (data namespaces are
/// deferred — ADR-0009).
#[derive(Debug)]
#[non_exhaustive]
pub struct IdentityStores {
    /// The bootstrap directory itself.
    pub private_metadata: PrivateMetadataStore,
    /// The connections store — created at provisioning, discovered through
    /// the directory at linking.
    pub connections: ConnectionsStore,
}

/// Provision an identity's store set on its first device: create the
/// connections store, publish its ticket into a fresh private-metadata
/// directory (under the key linking discovers it by), and register this
/// device in the device set.
///
/// The first-device counterpart of [`link_device`]: provisioning brings an
/// identity up from nothing, linking brings it up on every further device
/// from the seed — the directory ticket
/// ([`PrivateMetadataStore::share_ticket`]) the caller hands over out of
/// band. Data namespaces are not provisioned here; their discovery at
/// linking is deferred (ADR-0009).
pub async fn provision_identity(node: &mut SyncNode) -> Result<IdentityStores> {
    let connections = ConnectionsStore::create(node).await?;
    // Write access and dialable addresses: every device of the identity
    // writes to the shared store, and a linking device dials from the ticket.
    let connections_ticket = connections
        .share_ticket(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;

    let private_metadata = PrivateMetadataStore::create(node).await?;
    private_metadata
        .put_ticket(CONNECTIONS_TICKET, &connections_ticket)
        .await?;
    // Registration is immediate — the store is fresh, there is no first
    // sync for the local write to race.
    private_metadata.add_device(node.node_id()).await?;

    Ok(IdentityStores {
        private_metadata,
        connections,
    })
}

/// Link `node` as a device of the identity whose seed this is — the
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
    seed: DocTicket,
    timeout: Duration,
) -> Result<IdentityStores> {
    // Directory first — everything else is discovered through it.
    let private_metadata = PrivateMetadataStore::import(node, seed).await?;

    // Discover the connections-store ticket from the directory, then import it.
    let connections_ticket = wait_for_ticket(&private_metadata, CONNECTIONS_TICKET, timeout)
        .await?
        .context("connections ticket did not sync into the private metadata store")?;
    let connections = ConnectionsStore::import(node, connections_ticket).await?;

    // Join the device set so the identity's other devices see this one. Done
    // after catch-up so the local write doesn't race the import's first sync.
    private_metadata.add_device(node.node_id()).await?;

    Ok(IdentityStores {
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
