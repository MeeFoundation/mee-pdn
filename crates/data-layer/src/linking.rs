//! Device linking: bring an identity up on its first device, and every
//! further device up from a single seed.
//!
//! [`provision_identity`] is the first-device half: create the connections
//! store, publish its ticket into a fresh private-metadata directory,
//! register the device. [`link_device`] is the every-further-device half: it
//! takes the private-metadata-store ticket — the seed, as carried in a QR —
//! imports that directory, discovers and imports the identity's other
//! stores through it, then registers the new device in it — staying in
//! contact with the seed's devices from the import until the registration
//! has demonstrably reached one of the identity's existing devices, so no
//! phase rides on a single exchange. The private metadata store necessarily
//! comes first: every other store is found via a ticket stored inside it.
//!
//! Linking is per identity and repeatable: a node hosting several identities
//! runs it once per identity, each time with that identity's seed, and one
//! linking act imports nothing of any other identity. Adding an identity to
//! a device is always this explicit act — nothing cascades or propagates
//! automatically.
//!
//! The seed is a bearer ticket; identity-bound linking is future work.
//! Importing data namespaces during linking is deferred (ADR-0009).

use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use futures_lite::StreamExt;
use iroh::EndpointAddr;
use pdn_store::{
    api::protocol::{AddrInfoOptions, ShareMode},
    engine::LiveEvent,
    DocTicket,
};

use crate::connections::ConnectionsStore;
use crate::node::SyncNode;
use crate::private_metadata::PrivateMetadataStore;

/// Directory key under which the connections-store ticket lives.
const CONNECTIONS_TICKET: &str = "connections";

/// How often linking re-requests contact with the seed's devices while
/// waiting on sync delivery — during ticket discovery and the registration
/// push alike.
const CONTACT_RETRY_INTERVAL: Duration = Duration::from_millis(500);

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
pub async fn provision_identity(node: &SyncNode) -> Result<IdentityStores> {
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
/// Imports the directory, waits up to `timeout` for the connections-store
/// ticket to sync in — staying in contact with the seed's devices while it
/// waits (see [`wait_for_ticket`]) — imports the connections store, registers
/// this device in the directory ([`PrivateMetadataStore::add_device`] with
/// [`SyncNode::node_id`]), and finally confirms that registration reached
/// one of the identity's existing devices (see [`confirm_registration`]).
/// Replicating the stores' remaining contents is ongoing sync the caller
/// observes. A stalled directory — and a registration that cannot be
/// delivered — surfaces as a timeout error, not a hang.
pub async fn link_device(
    node: &SyncNode,
    seed: DocTicket,
    timeout: Duration,
) -> Result<IdentityStores> {
    // Where the directory's devices are reachable — the ticket wait and the
    // registration push below re-contact them.
    let seed_devices = seed.nodes.clone();

    // Directory first — everything else is discovered through it.
    let private_metadata = PrivateMetadataStore::import(node, seed).await?;

    // Discover the connections-store ticket from the directory, then import it.
    let connections_ticket = wait_for_ticket(
        &private_metadata,
        &seed_devices,
        CONNECTIONS_TICKET,
        timeout,
    )
    .await?
    .context("connections ticket did not sync into the private metadata store")?;
    let connections = ConnectionsStore::import(node, connections_ticket).await?;

    // Join the device set so the identity's other devices see this one. Done
    // after catch-up so the local write doesn't race the import's first sync.
    private_metadata.add_device(node.node_id()).await?;
    confirm_registration(&private_metadata, seed_devices, timeout).await?;

    Ok(IdentityStores {
        private_metadata,
        connections,
    })
}

/// Wait until the device registration written by [`link_device`] has
/// reached another device of the identity: keep the replica in contact
/// until one sync session that started after the write finishes
/// successfully — reconciliation exchanges both directions, so such a
/// session necessarily carried the registration out.
///
/// Without this, the registration rides on its one-shot gossip broadcast,
/// which races the swarm setup: fired before the neighborhood is up —
/// and after the import's initial reconciliation has already finished —
/// it reaches nobody, and with no later trigger the identity's other
/// devices would learn of this device only on some unrelated future
/// contact.
async fn confirm_registration(
    store: &PrivateMetadataStore,
    seed_devices: Vec<EndpointAddr>,
    timeout: Duration,
) -> Result<()> {
    let mut events = store.events().await?;
    // Sessions started after this instant see the registration in the
    // local replica; earlier ones may have missed it.
    let written_at = SystemTime::now();
    let deadline = Instant::now() + timeout;
    loop {
        // (Re-)request contact with the seed's devices. Idle replica: a
        // fresh session starts. Session already running: the request is
        // dropped — its finish event wakes the loop and the request is
        // repeated.
        store.sync_with(seed_devices.clone()).await?;

        let event = tokio::time::timeout(CONTACT_RETRY_INTERVAL, events.next()).await;
        if let Ok(event) = event {
            let event = event.context("replica event stream ended during linking")??;
            if let LiveEvent::SyncFinished(sync) = event {
                if sync.result.is_ok() && sync.started >= written_at {
                    return Ok(());
                }
            }
        }
        if Instant::now() > deadline {
            bail!("device registration did not reach the identity's devices in time");
        }
    }
}

/// Wait for the ticket of `kind` to sync into the directory, re-requesting
/// contact with the seed's devices on the way: delivery otherwise rides
/// solely on the import's initial exchange, and if that one exchange dies —
/// the peer transiently unreachable, the session aborted — nothing else
/// re-triggers it, and the wait would starve to its deadline with the
/// ticket one request away. The retry driver mirrors the registration
/// push's ([`confirm_registration`]).
async fn wait_for_ticket(
    store: &PrivateMetadataStore,
    seed_devices: &[EndpointAddr],
    kind: &str,
    timeout: Duration,
) -> Result<Option<DocTicket>> {
    let deadline = Instant::now() + timeout;
    let mut next_contact = Instant::now();
    loop {
        // (Re-)request contact on the push's cadence. A request dropped
        // against an already-running session is simply repeated once the
        // interval elapses.
        if Instant::now() >= next_contact {
            store.sync_with(seed_devices.to_vec()).await?;
            next_contact = Instant::now() + CONTACT_RETRY_INTERVAL;
        }
        if let Some(ticket) = store.get_ticket(kind).await? {
            return Ok(Some(ticket));
        }
        if Instant::now() > deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
