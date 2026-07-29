//! The runtime's write-retraction surface: the verdict consumer that turns
//! a tracker verdict into a durable directory marker plus an observable
//! event, and the marker sweep that acts on markers — its own and its
//! siblings' — by removing the addressed entries and arming their ingest
//! refusal. The verdict device only records; the sweep is the one act path
//! every device shares, so a retraction happens the same way wherever the
//! marker came from.

use std::{
    collections::HashSet,
    sync::Weak,
    time::{SystemTime, UNIX_EPOCH},
};

use data_layer::{AuthorId, RetractionMarker, RetractionVerdict};
use pdn_types::{EntryPath, NodeId, PdnId};
use tokio::sync::{mpsc, Mutex};

use crate::runtime::State;

/// One observed write retraction: the entry judged not accepted by the
/// issuer, addressed for the host to surface — and to recover from the
/// blob store, while the blob lives.
#[derive(Debug, Clone)]
pub struct RetractionEvent {
    /// The granted namespace's issuer that refused the write.
    pub issuer: PdnId,
    /// The path of the retracted entry.
    pub path: EntryPath,
    /// The retracted entry's author.
    pub author: AuthorId,
    /// The retracted entry's timestamp.
    pub timestamp: u64,
    /// The retracted entry's content hash.
    pub content_hash: [u8; 32],
    /// The device that reached the verdict.
    pub decided_by: NodeId,
}

/// Consume the node's verdict stream for the runtime's lifetime: each
/// verdict becomes one directory marker in the writing identity's
/// directory, one warning, and one event. The removal itself is not here —
/// the marker sweep performs it on every device alike, this one included,
/// so the verdict path and the sibling path cannot drift apart.
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

/// Record one verdict: resolve the namespace to its issuer and every hosted
/// identity whose grant binder holds it, write the marker in each of their
/// directories, warn, emit. One replica per issuer serves every identity
/// granted by it, and a marker replicates only to the devices of the identity
/// whose directory carries it, so the marker goes to all of them — picking
/// one would leave the others' devices holding the retracted entry. A verdict
/// that resolves to nothing — the namespace was forgotten while the verdict
/// travelled — is dropped: its entries left with the replica.
async fn record_verdict(state: &State, verdict: &RetractionVerdict, decided_by: NodeId) {
    let Ok(Some(issuer)) = state.node.issuer_of_namespace(verdict.namespace) else {
        return;
    };
    // Every identity granted by this issuer, each once: one identity can
    // hold the same grant over more than one connection.
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
    // The tracker honors only entries this runtime's writing surface
    // produced, and those keys are valid entry paths by construction.
    let Ok(path) = std::str::from_utf8(&verdict.key) else {
        return;
    };
    let Ok(path) = EntryPath::new(path) else {
        return;
    };
    // The verdict's fields come off the wire from the peer that refused the
    // write. Only the local record makes them true, and a marker is acted on
    // by removing entries at or below its bound on every device, so a name
    // this node cannot confirm records nothing.
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
    // Nothing durable was written, so nothing happened: the sweep is what
    // removes the entry, and it runs off the markers.
    if !recorded {
        return;
    }
    tracing::warn!(
        %issuer,
        path = %path,
        timestamp = verdict.timestamp,
        "write not accepted by the issuer; local copy retracted to the issuer's state"
    );
    // No receivers is not an error — the surface is optional to consume.
    let _unobserved = state.retraction_events.send(RetractionEvent {
        issuer,
        path,
        author: verdict.author,
        timestamp: verdict.timestamp,
        content_hash: *verdict.content_hash.as_bytes(),
        decided_by,
    });
}

/// How long a retraction marker lives (microseconds) before the retention-
/// window GC drops it — a coarse proxy for "the marked entry can no longer
/// win".
///
/// The window must outlast replication to the identity's own devices, because
/// the issuer's rejection backs up only the device that authored the entry: a
/// sibling holding a copy authored elsewhere is not this node's writer, and it
/// ignores rejections naming another author. A marker dropped before it
/// reached every sibling therefore leaves that copy free to flap back.
const MARKER_RETENTION_MICROS: u64 = 14 * 24 * 60 * 60 * 1_000_000;

/// Current time in microseconds since the Unix epoch — the unit of entry
/// timestamps, so a marker's directory-entry timestamp is comparable.
fn now_micros() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    u64::try_from(micros).unwrap_or(u64::MAX)
}

/// One marker sweep for `identity`: age out this device's stale markers, then
/// act on every readable marker in its directory — arm the ingest refusal and
/// remove the addressed entries locally. Idempotent by construction (arming
/// keeps the widest bound, removal of an absent record is a no-op), so it runs
/// on every directory change and every grant-binder sweep: whichever of the
/// marker and the namespace binding arrives second, the sweep after it acts.
pub(crate) async fn apply_retractions(state: &State, identity: PdnId) {
    let Ok(hosted) = state.hosted(identity) else {
        return;
    };
    // Retention-window GC: this device ages out the markers it recorded, and
    // takes down the refusal each one armed — arming only ever widens, so a
    // marker's disappearance is the one moment that can narrow it back.
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
        // An unbound issuer stays cold: the namespace the marker addresses
        // is not held here (yet) — the binder's own sweep re-runs this.
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
