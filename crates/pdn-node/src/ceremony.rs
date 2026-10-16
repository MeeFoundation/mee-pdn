//! What the linking dialogue (ADR-0012) and a pod's join (ADR-0015) share:
//! the pending one-time secrets, one length-prefixed framing over any
//! stream, and the failure of a dial that reached no inviting device.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use pdn_types::PdnId;
use rand::{rngs::SysRng, TryRng as _};
use serde::{de::DeserializeOwned, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    sync::Mutex,
};

use crate::runtime::State;

pub(crate) const DEFAULT_INVITE_LIFETIME: Duration = Duration::from_mins(2);

/// Ceiling on one length-prefixed wire message of both ceremonies, so a
/// malformed length prefix cannot demand an unbounded read.
pub(crate) const MAX_WIRE_MESSAGE_LEN: u32 = 64 * 1024;

/// The dial reached no inviting device — before any dialogue, so neither a
/// refusal nor a timeout. One type for both ceremonies. Downcast from the
/// `anyhow::Error` of `link` and `join`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("could not reach the inviter")]
pub struct InviterUnreachable;

#[derive(Debug, Clone, Copy)]
struct PendingInvite<T> {
    minted_for: T,
    expires_at: Instant,
}

/// One instance per ceremony that mints one-time secrets, inside the
/// runtime state so every operation is a map operation under the coarse
/// lock. Expiry is lazy: checked at presentation, swept at the next invite.
/// `T` is what a secret was minted for — an identity, or an identity and a
/// pod.
#[derive(Debug)]
pub(crate) struct PendingInvites<T = PdnId> {
    map: HashMap<[u8; 32], PendingInvite<T>>,
}

impl<T> Default for PendingInvites<T> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

impl<T: Copy> PendingInvites<T> {
    /// 32 bytes from the operating-system generator.
    pub(crate) fn mint(
        &mut self,
        minted_for: T,
        lifetime: Duration,
        now: Instant,
    ) -> Result<[u8; 32]> {
        let expires_at = now
            .checked_add(lifetime)
            .context("invite lifetime out of range")?;
        self.map.retain(|_, pending| pending.expires_at > now);
        let mut secret = [0u8; 32];
        SysRng
            .try_fill_bytes(&mut secret)
            .context("operating-system randomness unavailable")?;
        self.map.insert(
            secret,
            PendingInvite {
                minted_for,
                expires_at,
            },
        );
        Ok(secret)
    }

    /// Present and unexpired — burned and returned; expired — burned and
    /// refused; unknown — refused, burning nothing.
    pub(crate) fn verify_and_burn(&mut self, secret: &[u8; 32], now: Instant) -> Option<T> {
        // Peek first: a miss must not disturb the map.
        let live = self.map.get(secret)?.expires_at > now;
        let pending = self.map.remove(secret)?;
        live.then_some(pending.minted_for)
    }
}

/// Filled once, right after the node spawns (the handler is built before
/// the node exists), and held weakly so a handler clone does not keep the
/// state alive. A connection arriving before the slot is filled is refused.
pub(crate) type StateSlot = Arc<OnceLock<Weak<Mutex<State>>>>;

/// Generic over its stream, so one implementation serves the network and
/// the pipe two identities of one node meet over (ADR-0013).
pub(crate) async fn write_message<W: AsyncWrite + Unpin, T: Serialize>(
    send: &mut W,
    message: &T,
) -> Result<()> {
    let bytes = postcard::to_stdvec(message)?;
    let len = u32::try_from(bytes.len()).context("wire message too large")?;
    if len > MAX_WIRE_MESSAGE_LEN {
        anyhow::bail!("wire message too large: {len} bytes");
    }
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

/// Generic over its stream — see [`write_message`].
pub(crate) async fn read_message<R: AsyncRead + Unpin, T: DeserializeOwned>(
    recv: &mut R,
) -> Result<T> {
    let mut len_bytes = [0u8; 4];
    recv.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_WIRE_MESSAGE_LEN {
        anyhow::bail!("wire message too large: {len} bytes");
    }
    let mut bytes = vec![0u8; usize::try_from(len)?];
    recv.read_exact(&mut bytes).await?;
    Ok(postcard::from_bytes(&bytes)?)
}
