//! The host's configuration from the environment. Every value that is
//! present and unparseable fails rather than falling back: a mistyped bind
//! would come up on loopback and look like an unreachable peer, a mistyped
//! flag would look like a renamed route.

use std::net::{IpAddr, SocketAddr};

use anyhow::{Context as _, Result};
use pdn_node::Connectivity;

pub const DEFAULT_HOST: &str = "127.0.0.1";

pub const DEFAULT_PORT: u16 = 3011;

/// `None` is what an unset variable reads as.
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

/// `PDN_HOST` and `PDN_PORT`.
pub fn bind_addr_from_env() -> Result<SocketAddr> {
    let host = env("PDN_HOST")?;
    let port = env("PDN_PORT")?;
    bind_addr(host.as_deref(), port.as_deref())
}

/// `PDN_DEBUG`: a closed set of values, unset meaning off.
pub fn debug_enabled(raw: Option<&str>) -> Result<bool> {
    match raw {
        Some("1" | "true") => Ok(true),
        None | Some("0" | "false") => Ok(false),
        Some(other) => Err(anyhow::anyhow!(
            "PDN_DEBUG must be one of 1, true, 0, false, or unset — got {other:?}"
        )),
    }
}

pub fn debug_enabled_from_env() -> Result<bool> {
    debug_enabled(env("PDN_DEBUG")?.as_deref())
}

/// `PDN_CONNECTIVITY`: a closed set of values, unset meaning `direct`.
/// Every value past `direct` routes through infrastructure this project does
/// not run, so a host reaches it only when told to — the container stand
/// binds direct paths and the demonstration stand names `product`, whose
/// peers are devices that move between networks.
pub fn connectivity(raw: Option<&str>) -> Result<Connectivity> {
    match raw {
        None | Some("direct") => Ok(Connectivity::Direct),
        Some("relays") => Ok(Connectivity::Relays),
        Some("product") => Ok(Connectivity::RelaysAndAddressLookup),
        Some(other) => Err(anyhow::anyhow!(
            "PDN_CONNECTIVITY must be one of direct, relays, product, or unset — got {other:?}"
        )),
    }
}

pub fn connectivity_from_env() -> Result<Connectivity> {
    connectivity(env("PDN_CONNECTIVITY")?.as_deref())
}

/// `PDN_DATA_DIR`, required: a host started without a directory would
/// promise persistence while holding everything in RAM.
pub fn data_dir(raw: Option<&str>) -> Result<std::path::PathBuf> {
    match raw {
        Some(dir) if !dir.is_empty() => Ok(std::path::PathBuf::from(dir)),
        Some(_) | None => Err(anyhow::anyhow!(
            "PDN_DATA_DIR is not set — the host requires the runtime's storage directory \
             and offers no in-memory mode"
        )),
    }
}

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
    fn connectivity_is_direct_unless_named() {
        assert_eq!(connectivity(None).unwrap(), Connectivity::Direct);
        assert_eq!(connectivity(Some("direct")).unwrap(), Connectivity::Direct);
        assert_eq!(connectivity(Some("relays")).unwrap(), Connectivity::Relays);
        assert_eq!(
            connectivity(Some("product")).unwrap(),
            Connectivity::RelaysAndAddressLookup
        );
        assert!(connectivity(Some("relay")).is_err());
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
