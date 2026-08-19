//! Where the host listens.
//!
//! Loopback unless a wider bind is configured, so exposing the debug
//! surface beyond the local host is a deliberate act. The surface carries
//! live ceremony secrets and authenticates nobody: reaching it is reaching
//! the node.

use std::net::{IpAddr, SocketAddr};

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
    let host: IpAddr = host
        .parse()
        .with_context(|| format!("PDN_HOST is not an IP address: {host:?}"))?;
    Ok(SocketAddr::new(host, port))
}

fn env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err @ std::env::VarError::NotUnicode(_)) => {
            Err(err).with_context(|| format!("{name} is not valid Unicode"))
        }
    }
}

/// [`bind_addr`] over this process's own environment: `PDN_HOST` and
/// `PDN_PORT`.
pub fn bind_addr_from_env() -> Result<SocketAddr> {
    let host = env("PDN_HOST")?;
    let port = env("PDN_PORT")?;
    bind_addr(host.as_deref(), port.as_deref())
}

/// Resolve the debug-surface flag from `PDN_DEBUG`, `None` being unset —
/// the surface stays off. A closed set of values means on (`"1"`, `"true"`)
/// or off (`"0"`, `"false"`); anything else fails rather than falling back,
/// for the same reason [`bind_addr`] does: a typo that silently read as
/// "off" would turn the whole `/debug/` subtree into what looks like a
/// renamed route, not an unset flag.
pub fn debug_enabled(raw: Option<&str>) -> Result<bool> {
    match raw {
        Some("1" | "true") => Ok(true),
        None | Some("0" | "false") => Ok(false),
        Some(other) => Err(anyhow::anyhow!(
            "PDN_DEBUG must be one of 1, true, 0, false, or unset — got {other:?}"
        )),
    }
}

/// [`debug_enabled`] over this process's own environment: `PDN_DEBUG`.
pub fn debug_enabled_from_env() -> Result<bool> {
    debug_enabled(env("PDN_DEBUG")?.as_deref())
}

/// Resolve the runtime's storage directory from `PDN_DATA_DIR`, `None`
/// being unset. Required: the host exists for the container stand, offers
/// no in-memory mode and carries no path of its own — a host that started
/// without a directory would promise persistence while holding everything
/// in RAM, which from outside looks like a working node until its state is
/// gone.
pub fn data_dir(raw: Option<&str>) -> Result<std::path::PathBuf> {
    match raw {
        Some(dir) if !dir.is_empty() => Ok(std::path::PathBuf::from(dir)),
        Some(_) | None => Err(anyhow::anyhow!(
            "PDN_DATA_DIR is not set — the host requires the runtime's storage directory \
             and offers no in-memory mode"
        )),
    }
}

/// [`data_dir`] over this process's own environment: `PDN_DATA_DIR`.
pub fn data_dir_from_env() -> Result<std::path::PathBuf> {
    data_dir(env("PDN_DATA_DIR")?.as_deref())
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
    fn bare_ipv6_is_accepted() {
        assert_eq!(
            bind_addr(Some("::1"), None).unwrap(),
            "[::1]:3011".parse().unwrap()
        );
    }

    #[test]
    fn an_unparseable_value_fails_instead_of_defaulting() {
        assert!(bind_addr(None, Some("http")).is_err());
        assert!(bind_addr(Some("not a host"), None).is_err());
    }

    #[test]
    fn nothing_configured_is_off() {
        assert!(!debug_enabled(None).unwrap());
    }

    #[test]
    fn a_configured_value_is_taken_as_given() {
        assert!(debug_enabled(Some("1")).unwrap());
        assert!(debug_enabled(Some("true")).unwrap());
        assert!(!debug_enabled(Some("0")).unwrap());
        assert!(!debug_enabled(Some("false")).unwrap());
    }

    #[test]
    fn an_unrecognized_value_fails_instead_of_defaulting_to_off() {
        assert!(debug_enabled(Some("yes")).is_err());
        assert!(debug_enabled(Some("TRUE")).is_err());
        assert!(debug_enabled(Some(" 1")).is_err());
    }

    /// The host has no in-memory mode and no path of its own: unset — or
    /// set to nothing — stops the start, naming the variable.
    #[test]
    fn an_unset_data_dir_fails_naming_the_variable() {
        for raw in [None, Some("")] {
            let err = data_dir(raw).unwrap_err();
            assert!(
                err.to_string().contains("PDN_DATA_DIR"),
                "the refusal must name the variable: {err}"
            );
        }
    }

    #[test]
    fn a_configured_data_dir_is_taken_as_given() {
        assert_eq!(
            data_dir(Some("/var/lib/pdn")).unwrap(),
            std::path::PathBuf::from("/var/lib/pdn")
        );
    }
}
