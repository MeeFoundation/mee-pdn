use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Byte-backed ID infrastructure
// ---------------------------------------------------------------------------

/// Error returned when parsing a hex string into a 32-byte ID.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct ByteIdParseError {
    pub message: String,
}

/// Parse a lowercase hex string into `[u8; 32]`.
///
/// # Safety invariants (indexing)
/// - Length is checked to be exactly 64 before iteration.
/// - `chunks(2)` on a 64-byte slice yields exactly 32 chunks of 2 bytes each.
/// - `enumerate()` yields `i` in `0..32`, matching `out`'s bounds.
#[allow(clippy::indexing_slicing)]
pub fn parse_hex_32(s: &str) -> Result<[u8; 32], ByteIdParseError> {
    if s.len() != 64 {
        return Err(ByteIdParseError {
            message: format!("expected 64 hex chars, got {}", s.len()),
        });
    }
    let mut out = [0u8; 32];
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
macro_rules! define_byte_id {
    (
        $(#[$meta:meta])*
        $vis:vis struct $Name:ident;
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        $vis struct $Name([u8; 32]);

        impl $Name {
            /// Create from raw bytes.
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            /// View as raw bytes.
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl From<[u8; 32]> for $Name {
            fn from(b: [u8; 32]) -> Self {
                Self(b)
            }
        }

        impl AsRef<[u8; 32]> for $Name {
            fn as_ref(&self) -> &[u8; 32] {
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
                $crate::parse_hex_32(s).map(Self)
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

define_byte_id! {
    /// iroh endpoint identifier (ed25519 public key, 32 bytes).
    pub struct NodeId;
}

// -- KERI identity types ----------------------------------------------------

define_byte_id! {
    /// KERI Autonomic Identifier (ed25519 inception public key, 32 bytes).
    ///
    /// A self-certifying root identifier that never changes even as
    /// operational keys rotate. Derived from the ed25519 public key
    /// present in the KERI inception event.
    pub struct Aid;
}

// -- Roadmap placeholders ---------------------------------------------------

// TODO: Integrate into identity/trust layer once persona management
// is implemented. Currently defined but unused.
/// Classification of a persona's visibility scope.
/// Maps to the First Person Network's P-DID / C-DID / U-DID concepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PersonaKind {
    /// Public persona (P-DID). Visible to anyone.
    Public,
    /// Community persona (C-DID). Shared within a specific community.
    Community,
    /// Private persona (U-DID). Shared only with specific parties.
    Private,
}
