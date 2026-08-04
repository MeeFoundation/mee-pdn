//! Where the host listens.
//!
//! Loopback unless a wider bind is configured, so exposing the debug
//! surface beyond the local host is a deliberate act. The surface carries
//! live ceremony secrets and authenticates nobody: reaching it is reaching
//! the node.

use std::net::SocketAddr;

use anyhow::{Context as _, Result};

/// The bind host when `PDN_HOST` is unset.
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// The bind port when `PDN_PORT` is unset.
pub const DEFAULT_PORT: u16 = 3011;

/// Resolve the bind address from the two configured values, `None` being
/// what an unset variable reads as. A value that is present and
/// unparseable fails rather than falling back: a container that meant to
/// bind wider and mistyped it would otherwise come up on loopback and look
/// like an unreachable peer.
pub fn bind_addr(host: Option<&str>, port: Option<&str>) -> Result<SocketAddr> {
    let host = host.unwrap_or(DEFAULT_HOST);
    let port: u16 = match port {
        Some(port) => port
            .parse()
            .with_context(|| format!("PDN_PORT is not a port number: {port:?}"))?,
        None => DEFAULT_PORT,
    };
    format!("{host}:{port}")
        .parse()
        .with_context(|| format!("not a bind address: {host}:{port}"))
}

/// [`bind_addr`] over this process's own environment: `PDN_HOST` and
/// `PDN_PORT`.
pub fn bind_addr_from_env() -> Result<SocketAddr> {
    let host = std::env::var("PDN_HOST").ok();
    let port = std::env::var("PDN_PORT").ok();
    bind_addr(host.as_deref(), port.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_configured_binds_loopback() {
        let addr = bind_addr(None, None).unwrap();
        assert_eq!(addr.to_string(), "127.0.0.1:3011");
        assert!(addr.ip().is_loopback(), "the default must not be reachable");
    }

    /// A wider bind is exactly what was asked for, and nothing else.
    #[test]
    fn a_configured_bind_is_taken_as_given() {
        let addr = bind_addr(Some("0.0.0.0"), Some("8080")).unwrap();
        assert_eq!(addr.to_string(), "0.0.0.0:8080");
        assert!(!addr.ip().is_loopback());
    }

    #[test]
    fn a_configured_port_alone_stays_on_loopback() {
        assert_eq!(
            bind_addr(None, Some("9000")).unwrap().to_string(),
            "127.0.0.1:9000"
        );
    }

    #[test]
    fn an_unparseable_value_fails_instead_of_defaulting() {
        assert!(bind_addr(None, Some("http")).is_err());
        assert!(bind_addr(Some("not a host"), None).is_err());
    }
}
