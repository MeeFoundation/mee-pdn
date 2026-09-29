use serde::{Deserialize, Serialize};
use thiserror::Error;

mod cell;
mod data;
mod non_empty;
pub use cell::{CellId, RecordId, RecordKind, RecordRef, UnknownRecordKind};
pub use data::{EntryInfo, EntryPath, NamespaceRole, NodeAddr, PathValidationError};
pub use non_empty::NonEmpty;

// ---------------------------------------------------------------------------
// Byte-backed ID infrastructure
// ---------------------------------------------------------------------------

/// Error returned when parsing a hex string into a byte ID.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct ByteIdParseError {
    pub message: String,
}

/// Parse a lowercase hex string into `[u8; N]`.
///
/// # Safety invariants (indexing)
/// - Length is checked to be exactly `2 * N` before iteration.
/// - `chunks(2)` on a `2 * N`-byte slice yields exactly `N` chunks of 2 bytes each.
/// - `enumerate()` yields `i` in `0..N`, matching `out`'s bounds.
#[allow(clippy::indexing_slicing)]
pub fn parse_hex<const N: usize>(s: &str) -> Result<[u8; N], ByteIdParseError> {
    if s.len() != 2 * N {
        return Err(ByteIdParseError {
            message: format!("expected {} hex chars, got {}", 2 * N, s.len()),
        });
    }
    let mut out = [0u8; N];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

#[allow(clippy::as_conversions)]
fn hex_digit(b: u8) -> Result<u8, ByteIdParseError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        // REASON: u8 -> char is a safe widening cast for error display.
        _ => Err(ByteIdParseError {
            message: format!("invalid hex digit: {}", b as char),
        }),
    }
}

/// Define a newtype wrapping `[u8; 32]` with hex Display/FromStr and serde.
#[macro_export]
macro_rules! define_byte_id_32 {
    (
        $(#[$meta:meta])*
        $vis:vis struct $Name:ident;
    ) => {
        $crate::__define_byte_id! { 32; $(#[$meta])* $vis struct $Name; }
    };
}

/// Define a newtype wrapping `[u8; 16]` with hex Display/FromStr and serde.
#[macro_export]
macro_rules! define_byte_id_16 {
    (
        $(#[$meta:meta])*
        $vis:vis struct $Name:ident;
    ) => {
        $crate::__define_byte_id! { 16; $(#[$meta])* $vis struct $Name; }
    };
}

/// The body of `define_byte_id_32!` and `define_byte_id_16!`.
#[doc(hidden)]
#[macro_export]
macro_rules! __define_byte_id {
    (
        $N:literal;
        $(#[$meta:meta])*
        $vis:vis struct $Name:ident;
    ) => {
        $(#[$meta])*
        // `Ord` is byte order and nothing else: it ranks no identity above
        // another, and no rule may be written in terms of it. It exists so
        // these ids key ordered collections and settle ties — which of two
        // sides a memo is written from, which of two dials survives — where
        // the choice must only be the same on both sides and at both ends.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $Name([u8; $N]);

        impl $Name {
            /// Create from raw bytes.
            pub const fn from_bytes(bytes: [u8; $N]) -> Self {
                Self(bytes)
            }

            /// View as raw bytes.
            pub const fn as_bytes(&self) -> &[u8; $N] {
                &self.0
            }
        }

        impl From<[u8; $N]> for $Name {
            fn from(b: [u8; $N]) -> Self {
                Self(b)
            }
        }

        impl AsRef<[u8; $N]> for $Name {
            fn as_ref(&self) -> &[u8; $N] {
                &self.0
            }
        }

        impl ::std::fmt::Display for $Name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                for byte in &self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl ::std::fmt::Debug for $Name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}(", stringify!($Name))?;
                for byte in &self.0[..4] {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, "...)")
            }
        }

        impl ::std::str::FromStr for $Name {
            type Err = $crate::ByteIdParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                $crate::parse_hex(s).map(Self)
            }
        }

        impl ::serde::Serialize for $Name {
            fn serialize<S: ::serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
                ser.serialize_str(&self.to_string())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $Name {
            fn deserialize<D: ::serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                let s = <String as ::serde::Deserialize>::deserialize(de)?;
                s.parse().map_err(::serde::de::Error::custom)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

define_byte_id_32! {
    /// iroh endpoint identifier (ed25519 public key, 32 bytes).
    pub struct NodeId;
}

// -- PDN identity ----------------------------------------------------------

define_byte_id_32! {
    /// Stable identifier of a participant on the PDN.
    ///
    /// Used at the PDN domain layer (claims, connections, delegation) so
    /// that higher-level code does not depend on identity-implementation
    /// details such as KERI `Aid`.
    pub struct PdnId;
}

/// Cryptographic evidence that a particular `PdnId` issued a statement.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PdnIdentityProof {}

// -- KERI identity types ----------------------------------------------------

define_byte_id_32! {
    /// KERI Autonomic Identifier (ed25519 inception public key, 32 bytes).
    ///
    /// A self-certifying root identifier that never changes even as
    /// operational keys rotate. Derived from the ed25519 public key
    /// present in the KERI inception event.
    pub struct Aid;
}

define_byte_id_32! {
    /// Current ed25519 operational signing key from the latest KEL event.
    ///
    /// Changes on key rotation (unlike `Aid`, which is permanent).
    pub struct OperationalKey;
}

define_byte_id_32! {
    /// Stable identifier of a claim in the PDN domain layer.
    ///
    /// Used as the resource in `UWill` capability tokens (`res`), so that
    /// capabilities reference domain-level claims rather than
    /// storage-level locations.
    pub struct ClaimId;
}
