//! The addresses the server's three listeners bind.
//!
//! Each listener binds every interface unless its variable names one address.
//! The values are read once at startup, before anything else runs, so a typo
//! stops the server with the variable's name rather than leaving one listener
//! on an address nobody asked for.

use aurcache_common::server_env::{AURCACHE_HTTP_IP, AURCACHE_MIRROR_IP, AURCACHE_WORKER_IP, Var};
use std::net::{IpAddr, Ipv4Addr};

/// What a listener binds when its variable is unset: every IPv4 interface.
pub const DEFAULT_BIND_ADDRESS: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

/// Where each listener accepts connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindAddresses {
    /// The web UI and the API, `AURCACHE_HTTP_IP`.
    pub api: IpAddr,
    /// The pacman repository, `AURCACHE_MIRROR_IP`.
    pub repo: IpAddr,
    /// The worker protocol, `AURCACHE_WORKER_IP`.
    pub worker: IpAddr,
}

impl BindAddresses {
    /// The addresses the environment names, the default for any it leaves
    /// unset or empty.
    pub fn from_env() -> Result<Self, InvalidBindAddress> {
        Ok(Self {
            api: read(AURCACHE_HTTP_IP)?,
            repo: read(AURCACHE_MIRROR_IP)?,
            worker: read(AURCACHE_WORKER_IP)?,
        })
    }
}

/// A variable whose value is not an IP address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidBindAddress {
    pub var: Var,
    pub value: String,
}

impl std::fmt::Display for InvalidBindAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is `{}`, which is not an IP address; expected one such as `0.0.0.0`, \
             `127.0.0.1`, `::` or `::1`, without a port",
            self.var, self.value
        )
    }
}

impl std::error::Error for InvalidBindAddress {}

fn read(var: Var) -> Result<IpAddr, InvalidBindAddress> {
    parse(var, var.get().ok().as_deref())
}

/// `value` as `var`'s address. Unset and blank both mean the default, so a
/// compose file can pass the variable through without a value of its own.
fn parse(var: Var, value: Option<&str>) -> Result<IpAddr, InvalidBindAddress> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(DEFAULT_BIND_ADDRESS);
    };
    value.parse().map_err(|_| InvalidBindAddress {
        var,
        value: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_BIND_ADDRESS, InvalidBindAddress, parse};
    use aurcache_common::server_env::AURCACHE_HTTP_IP;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn ok(value: &str) -> IpAddr {
        parse(AURCACHE_HTTP_IP, Some(value)).expect("an address")
    }

    #[test]
    fn unset_or_blank_is_every_ipv4_interface() {
        assert_eq!(DEFAULT_BIND_ADDRESS, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(parse(AURCACHE_HTTP_IP, None), Ok(DEFAULT_BIND_ADDRESS));
        assert_eq!(ok(""), DEFAULT_BIND_ADDRESS);
        assert_eq!(ok("  "), DEFAULT_BIND_ADDRESS);
    }

    #[test]
    fn takes_ipv4() {
        assert_eq!(ok("127.0.0.1"), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(
            ok(" 192.168.1.20 "),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))
        );
    }

    #[test]
    fn takes_ipv6() {
        assert_eq!(ok("::"), IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert_eq!(ok("::1"), IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(
            ok("fd00::20"),
            IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x20))
        );
    }

    /// A host name, a port or brackets are refused rather than ignored, and
    /// the refusal names the variable so the operator knows which to fix.
    #[test]
    fn refuses_anything_else_naming_the_variable() {
        for value in ["localhost", "0.0.0.0:8080", "[::1]", "300.1.1.1", "eth0"] {
            let err = parse(AURCACHE_HTTP_IP, Some(value)).unwrap_err();
            assert_eq!(
                err,
                InvalidBindAddress {
                    var: AURCACHE_HTTP_IP,
                    value: value.to_string(),
                }
            );
            let message = err.to_string();
            assert!(message.starts_with("AURCACHE_HTTP_IP is `"), "{message}");
            assert!(message.contains(value), "{message}");
        }
    }
}
