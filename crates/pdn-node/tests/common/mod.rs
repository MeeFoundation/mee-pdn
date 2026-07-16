//! Helpers shared by this crate's scenario tests.
//!
//! Runtime-level test support lives here rather than in `test-utils`: that
//! crate serves the layer below (`data-layer`'s own tests depend on it), so a
//! helper needing `Runtime` would force it to depend on `pdn-node` and make
//! the lower layer's tests compile the runtime above them.

use std::time::{Duration, Instant};

use anyhow::Result;
use pdn_node::{ConnectionsService as _, InvitePayload, Runtime};
use pdn_types::PdnId;
use test_utils::TIMEOUT;

/// Establish with patience for a cold transport: present `invite`, and on
/// failure keep retrying with a **freshly minted** invite until [`TIMEOUT`].
///
/// A fresh process's first dials can be slowed or dropped — the cold-start
/// costs described on [`test_utils::TIMEOUT`] — so success-path
/// establishments retry. The retry mints a new invite rather than replaying
/// the old secret: the inviter burns the secret before replying (ADR-0011),
/// so a failure at or after the burn is recoverable only by a fresh invite —
/// re-presenting the same one would be refused forever. This mirrors the
/// QR-rotation a host performs (the scanner captures the currently
/// displayed, freshly minted invite). Assertions about one specific invite —
/// that *this* secret is refused, or must be the one burned — call
/// `establish` directly instead.
pub async fn establish_patiently(
    scanner: &Runtime,
    scanner_id: PdnId,
    inviter: &Runtime,
    inviter_id: PdnId,
    mut invite: InvitePayload,
) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match scanner.connections().establish(scanner_id, invite).await {
            Ok(()) => return Ok(()),
            Err(err) if Instant::now() > deadline => return Err(err),
            Err(_cold_or_burned) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                invite = inviter.connections().invite(inviter_id, None).await?;
            }
        }
    }
}
