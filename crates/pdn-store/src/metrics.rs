//! The two values of the sync actor's session snapshots, which the codec's
//! tests read.

use iroh_metrics::{Counter, Gauge};

/// What the sync actor counts of the snapshots its sessions hold.
#[derive(Debug, Default)]
pub(crate) struct Metrics {
    /// Sync sessions holding a store snapshot right now. A zero does not
    /// say that no read transaction is open: `get_many` and `content_hashes`
    /// hold one for as long as their iterator lives, and neither is counted
    /// here.
    pub(crate) sync_sessions_open: Gauge,
    /// Sync session snapshots the actor reclaimed by itself, because the
    /// handle was gone and no release message arrived: a release lost to a
    /// full actor queue, or a registration whose handle a cancelled caller
    /// never received.
    pub(crate) sync_sessions_reclaimed: Counter,
}
