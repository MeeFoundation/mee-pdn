//! What the linking dialogue (ADR-0012) and a pod's join (ADR-0015) share:
//! the pending one-time secrets, one length-prefixed framing over any
//! stream, and the failure of a dial that reached no inviting device.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
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

/// What became of an invite on the runtime that minted it; the pods spec
/// and the device-linking spec hold when each state is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteState<N> {
    Pending,
    /// Presented and burned; its dialogue in flight or cut short.
    Burned,
    /// Its serving half handed this newcomer what it came for.
    Used(N),
    /// Never presented, its lifetime over.
    Expired,
    /// Not held for what the question names: never minted here, minted
    /// for another identity or pod, swept, or minted before this start.
    Unknown,
}

#[derive(Debug, Clone, Copy)]
enum Fate<N> {
    Pending,
    Burned,
    Used(N),
}

#[derive(Debug, Clone, Copy)]
struct PendingInvite<T, N> {
    minted_for: T,
    expires_at: Instant,
    fate: Fate<N>,
}

/// One instance per ceremony that mints one-time secrets, inside the
/// runtime state so every operation is a map operation under the coarse
/// lock. Expiry is lazy: checked at presentation, swept at the next invite.
/// A presented secret stays, marked, until that sweep. `T` is what a secret
/// was minted for — an identity, or an identity and a pod; `N` names the
/// newcomer it was used by.
#[derive(Debug)]
pub(crate) struct PendingInvites<T, N> {
    map: HashMap<[u8; 32], PendingInvite<T, N>>,
}

impl<T, N> Default for PendingInvites<T, N> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

impl<T: Copy + PartialEq, N: Copy> PendingInvites<T, N> {
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
                fate: Fate::Pending,
            },
        );
        Ok(secret)
    }

    /// Pending and unexpired — marked burned and returned; anything else
    /// refused, changing nothing.
    pub(crate) fn verify_and_burn(&mut self, secret: &[u8; 32], now: Instant) -> Option<T> {
        let pending = self.map.get_mut(secret)?;
        if !matches!(pending.fate, Fate::Pending) || pending.expires_at <= now {
            return None;
        }
        pending.fate = Fate::Burned;
        Some(pending.minted_for)
    }

    /// Called once the serving half's last write to `newcomer` returned; a
    /// secret swept meanwhile stays unknown.
    pub(crate) fn mark_used(&mut self, secret: &[u8; 32], newcomer: N) {
        if let Some(pending) = self.map.get_mut(secret) {
            if matches!(pending.fate, Fate::Burned) {
                pending.fate = Fate::Used(newcomer);
            }
        }
    }

    pub(crate) fn state(&self, secret: &[u8; 32], minted_for: T, now: Instant) -> InviteState<N> {
        match self.map.get(secret) {
            Some(pending) if pending.minted_for == minted_for => match pending.fate {
                Fate::Pending if pending.expires_at <= now => InviteState::Expired,
                Fate::Pending => InviteState::Pending,
                Fate::Burned => InviteState::Burned,
                Fate::Used(newcomer) => InviteState::Used(newcomer),
            },
            _ => InviteState::Unknown,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const LIFETIME: Duration = Duration::from_secs(120);

    fn invites() -> PendingInvites<u8, char> {
        PendingInvites::default()
    }

    /// A presented secret is marked burned, then used by its newcomer, and a
    /// second presentation is refused and leaves it used.
    #[test]
    fn a_presented_secret_reads_burned_then_used() -> Result<()> {
        let now = Instant::now();
        let mut set = invites();
        let secret = set.mint(1, LIFETIME, now)?;
        assert_eq!(set.state(&secret, 1, now), InviteState::Pending);
        assert_eq!(set.verify_and_burn(&secret, now), Some(1));
        assert_eq!(set.state(&secret, 1, now), InviteState::Burned);
        set.mark_used(&secret, 'c');
        assert_eq!(set.state(&secret, 1, now), InviteState::Used('c'));
        // A replay is refused and changes nothing.
        assert_eq!(set.verify_and_burn(&secret, now), None);
        assert_eq!(set.state(&secret, 1, now), InviteState::Used('c'));
        Ok(())
    }

    /// A secret never presented reads expired once its lifetime ends, is
    /// refused then without a change, and reads unknown once a mint swept it.
    #[test]
    fn an_unpresented_secret_expires_and_is_swept() -> Result<()> {
        let now = Instant::now();
        let later = now + LIFETIME + Duration::from_secs(1);
        let mut set = invites();
        let secret = set.mint(1, LIFETIME, now)?;
        assert_eq!(set.state(&secret, 1, later), InviteState::Expired);
        assert_eq!(set.verify_and_burn(&secret, later), None);
        assert_eq!(set.state(&secret, 1, later), InviteState::Expired);
        set.mint(1, LIFETIME, later)?;
        assert_eq!(set.state(&secret, 1, later), InviteState::Unknown);
        Ok(())
    }

    /// A refused presentation of a never-minted secret leaves a pending one
    /// pending and live; a used one is swept with its lifetime too.
    #[test]
    fn a_refused_presentation_changes_nothing() -> Result<()> {
        let now = Instant::now();
        let mut set = invites();
        let pending = set.mint(1, LIFETIME, now)?;
        assert_eq!(set.verify_and_burn(&[7; 32], now), None);
        assert_eq!(set.state(&[7; 32], 1, now), InviteState::Unknown);
        assert_eq!(set.state(&pending, 1, now), InviteState::Pending);
        // Marking a secret no presentation burned leaves it pending.
        set.mark_used(&pending, 'x');
        assert_eq!(set.state(&pending, 1, now), InviteState::Pending);
        assert_eq!(set.verify_and_burn(&pending, now), Some(1));
        set.mark_used(&pending, 'c');
        let later = now + LIFETIME + Duration::from_secs(1);
        set.mint(1, LIFETIME, later)?;
        assert_eq!(set.state(&pending, 1, later), InviteState::Unknown);
        Ok(())
    }

    /// A secret is answered only for what it was minted for.
    #[test]
    fn a_secret_reads_unknown_for_anything_else() -> Result<()> {
        let now = Instant::now();
        let mut set = invites();
        let secret = set.mint(1, LIFETIME, now)?;
        assert_eq!(set.state(&secret, 2, now), InviteState::Unknown);
        assert_eq!(set.state(&secret, 1, now), InviteState::Pending);
        Ok(())
    }
}
