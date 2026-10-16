//! The armer: one sweep per change of a hosted identity's PMS and per sweep
//! interval, confirming this device and opening the pods the PMS records.
//! Started by `admit_hosted` in `runtime.rs`, the one step that hosts an
//! identity.

use std::sync::Weak;

use anyhow::Result;
use futures_lite::{Stream, StreamExt as _};
use pdn_types::PdnId;
use tokio::sync::Mutex;

use crate::runtime::State;

/// Keep hosted `identity`'s device confirmed and its pods open: one sweep
/// now, then one per PMS change and one per sweep interval. Holds the state
/// weakly and upgrades per sweep, so the task ends with the runtime instead
/// of keeping it alive.
pub(crate) fn spawn_identity_armer(
    state: Weak<Mutex<State>>,
    identity: PdnId,
    changes: impl Stream<Item = Result<()>> + Send + Unpin + 'static,
) {
    let mut changes = changes;
    let _detached = tokio::spawn(async move {
        loop {
            let interval = {
                let Some(strong) = state.upgrade() else {
                    return;
                };
                let mut guard = strong.lock().await;
                if !take_buffered(&mut changes).await {
                    return;
                }
                if let Err(err) = ensure_own_device_confirmed(&mut guard, identity).await {
                    tracing::warn!(%identity, "confirming this device in the PMS failed: {err:#}");
                }
                crate::pods::arm_pods(&guard, identity).await;
                guard.sweep_interval
            };
            // Two wake sources: a sweep can fail for a reason the PMS
            // knows nothing about (an import that met a full disk), and
            // nothing then writes to the PMS to ask for another try.
            tokio::select! {
                change = changes.next() => match change {
                    Some(Ok(())) => {}
                    Some(Err(_)) | None => return,
                },
                () = tokio::time::sleep(interval) => {}
            }
        }
    });
}

/// Consumes every change already buffered, taken under the lock, so the
/// sweep after it covers them all and a burst costs one sweep; `false` once
/// the subscription failed or ended.
pub(crate) async fn take_buffered(changes: &mut (impl Stream<Item = Result<()>> + Unpin)) -> bool {
    loop {
        match futures_lite::future::poll_once(changes.next()).await {
            None => return true,
            Some(Some(Ok(()))) => {}
            Some(Some(Err(_)) | None) => return false,
        }
    }
}

/// Write this node's device record when the confirmed set lacks it — the
/// repair for a link whose confirmation failed after its hosting was
/// recorded, and for a kill between the two. Because of this write, device
/// removal cannot be the absence of the record: it has to be a record of
/// its own that this write consults, and the runtime has none.
pub(crate) async fn ensure_own_device_confirmed(state: &mut State, identity: PdnId) -> Result<()> {
    let own_device = state.node.node_id();
    let hosted = state.hosted(identity)?;
    if hosted.pms.list_devices().await?.contains(&own_device) {
        return Ok(());
    }
    hosted.pms.confirm_device(own_device).await
}
