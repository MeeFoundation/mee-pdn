//! Write retraction at the runtime: the verdict consumer records a
//! directory marker and emits an event; the marker sweep — the one act path
//! every device shares — removes the addressed entries and arms their
//! ingest refusal.

use std::{
    collections::HashSet,
    sync::Weak,
    time::{SystemTime, UNIX_EPOCH},
};

use data_layer::{AuthorId, RetractionMarker, RetractionVerdict};
use pdn_types::{EntryPath, NodeId, PdnId};
use tokio::sync::{mpsc, Mutex};

use crate::runtime::State;

/// One retracted entry, addressed for the host to surface — and to recover
/// from the blob store while the blob lives.
#[derive(Debug, Clone)]
pub struct RetractionEvent {
    pub issuer: PdnId,
    pub path: EntryPath,
    pub author: AuthorId,
    pub timestamp: u64,
    pub content_hash: [u8; 32],
    pub decided_by: NodeId,
}

/// Each verdict becomes a directory marker, a warning, and an event. The
/// removal is the marker sweep's, on every device alike, so the verdict
/// path and the sibling path cannot drift apart.
pub(crate) fn spawn_retraction_consumer(
    state: Weak<Mutex<State>>,
    mut verdicts: mpsc::UnboundedReceiver<RetractionVerdict>,
    decided_by: NodeId,
) {
    let _detached = tokio::spawn(async move {
        while let Some(verdict) = verdicts.recv().await {
            let Some(strong) = state.upgrade() else {
                return;
            };
            let guard = strong.lock().await;
            record_verdict(&guard, &verdict, decided_by).await;
        }
    });
}

/// The marker goes into the directory of every hosted identity granted by
/// the issuer: one replica serves them all, and a marker reaches only the
/// devices of the identity whose directory carries it.
async fn record_verdict(state: &State, verdict: &RetractionVerdict, decided_by: NodeId) {
    let Ok(Some(issuer)) = state.node.issuer_of_namespace(verdict.namespace) else {
        return;
    };
    let identities: HashSet<PdnId> = state
        .bound_grants
        .keys()
        .filter(|(_identity, _peer, bound_issuer)| *bound_issuer == issuer)
        .map(|(identity, _peer, _issuer)| *identity)
        .collect();
    if identities.is_empty() {
        tracing::warn!(
            %issuer,
            "a write was not accepted, but no hosted identity holds a grant on that issuer any \
             more; nothing is recorded"
        );
        return;
    }
    let Ok(path) = std::str::from_utf8(&verdict.key) else {
        return;
    };
    let Ok(path) = EntryPath::new(path) else {
        return;
    };
    // The verdict's fields are the refusing peer's word; only the local
    // record makes them true.
    if !matches!(
        state.node.holds_rejected_entry(issuer, verdict).await,
        Ok(true)
    ) {
        return;
    }
    let marker = RetractionMarker {
        bound: verdict.timestamp,
        decided_by,
        content_hash: *verdict.content_hash.as_bytes(),
        timestamp: verdict.timestamp,
    };
    let mut recorded = false;
    for identity in identities {
        let Ok(hosted) = state.hosted(identity) else {
            continue;
        };
        match hosted
            .directory
            .record_retraction(issuer, verdict.author, path.as_str(), &marker)
            .await
        {
            Ok(()) => recorded = true,
            Err(err) => {
                tracing::warn!(%issuer, path = %path, "failed to record retraction marker: {err:#}");
            }
        }
    }
    // Nothing durable was written, so nothing happened.
    if !recorded {
        return;
    }
    tracing::warn!(
        %issuer,
        path = %path,
        timestamp = verdict.timestamp,
        "write not accepted by the issuer; local copy retracted to the issuer's state"
    );
    let _unobserved = state.retraction_events.send(RetractionEvent {
        issuer,
        path,
        author: verdict.author,
        timestamp: verdict.timestamp,
        content_hash: *verdict.content_hash.as_bytes(),
        decided_by,
    });
}

/// Marker retention (microseconds). Must outlast replication to the
/// identity's own devices: the issuer's rejection backs up only the device
/// that authored the entry, so a marker dropped before it reached every
/// sibling leaves a copy free to flap back.
const MARKER_RETENTION_MICROS: u64 = 14 * 24 * 60 * 60 * 1_000_000;

/// Microseconds since the Unix epoch — the unit of entry timestamps.
fn now_micros() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    u64::try_from(micros).unwrap_or(u64::MAX)
}

/// One marker sweep: age out this device's stale markers, then arm and
/// remove for every readable marker. Idempotent, so it runs on every
/// directory change and every grant-binder sweep — whichever of the marker
/// and the namespace binding arrives second, the sweep after it acts.
pub(crate) async fn apply_retractions(state: &State, identity: PdnId) {
    let Ok(hosted) = state.hosted(identity) else {
        return;
    };
    if let Ok(dropped) = hosted
        .directory
        .prune_aged_retractions(now_micros(), MARKER_RETENTION_MICROS)
        .await
    {
        for (issuer, author, path) in dropped {
            let _unbound = state
                .node
                .disarm_retraction(issuer, author, path.as_bytes());
        }
    }
    let Ok(markers) = hosted.directory.list_retractions().await else {
        return;
    };
    for (issuer, author, path, marker) in markers {
        // An unbound issuer stays cold until the binder's sweep re-runs this.
        if state
            .node
            .arm_retraction(issuer, author, path.clone().into_bytes(), marker.bound)
            .is_err()
        {
            continue;
        }
        let _already_gone = state
            .node
            .retract_entry(issuer, author, path.as_bytes(), marker.bound)
            .await;
    }
}
