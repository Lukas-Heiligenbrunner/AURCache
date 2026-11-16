//! Where the server's three listeners accept connections.
//!
//! Each listener binds every interface on its default port unless its
//! variables say otherwise. The values are read once at startup, before
//! anything else runs, so a typo stops the server with the variable's name
//! rather than leaving one listener somewhere nobody asked for.

use aurcache_common::ports;
use aurcache_common::server_env::{
    AURCACHE_HTTP_LISTEN_ADDRESS, AURCACHE_HTTP_PORT, AURCACHE_REPO_LISTEN_ADDRESS,
    AURCACHE_REPO_PORT, AURCACHE_WORKER_LISTEN_ADDRESS, AURCACHE_WORKER_PORT, Var,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// What a listener binds when its variable is unset: every IPv4 interface.
pub const DEFAULT_LISTEN_ADDRESS: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

/// Where each listener accepts connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenAddresses {
    /// The web UI and the API: `AURCACHE_HTTP_LISTEN_ADDRESS`,
    /// `AURCACHE_HTTP_PORT`.
    pub api: SocketAddr,
    /// The pacman repository: `AURCACHE_REPO_LISTEN_ADDRESS`,
    /// `AURCACHE_REPO_PORT`.
    pub repo: SocketAddr,
    /// The worker protocol: `AURCACHE_WORKER_LISTEN_ADDRESS`,
    /// `AURCACHE_WORKER_PORT`.
    pub worker: SocketAddr,
}

impl Default for ListenAddresses {
    /// Every IPv4 interface, on the default ports.
    fn default() -> Self {
        Self {
            api: SocketAddr::new(DEFAULT_LISTEN_ADDRESS, ports::AURCACHE_HTTP_PORT),
            repo: SocketAddr::new(DEFAULT_LISTEN_ADDRESS, ports::AURCACHE_REPO_PORT),
            worker: SocketAddr::new(DEFAULT_LISTEN_ADDRESS, ports::AURCACHE_WORKER_PORT),
        }
    }
}

impl ListenAddresses {
    /// What the environment names, the default for anything it leaves unset
    /// or empty.
    pub fn from_env() -> Result<Self, InvalidListenSetting> {
        let defaults = Self::default();
        Ok(Self {
            api: read(
                AURCACHE_HTTP_LISTEN_ADDRESS,
                AURCACHE_HTTP_PORT,
                defaults.api,
            )?,
            repo: read(
                AURCACHE_REPO_LISTEN_ADDRESS,
                AURCACHE_REPO_PORT,
                defaults.repo,
            )?,
            worker: read(
                AURCACHE_WORKER_LISTEN_ADDRESS,
                AURCACHE_WORKER_PORT,
                defaults.worker,
            )?,
        })
    }
}

/// What a listener variable has to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    /// An IP address, such as `0.0.0.0` or `::1`.
    Address,
    /// A port from 1 to 65535.
    Port,
}

/// A listener variable whose value is not what it has to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidListenSetting {
    pub var: Var,
    pub value: String,
    pub expected: Expected,
}

impl std::fmt::Display for InvalidListenSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            var,
            value,
            expected,
        } = self;
        match expected {
            Expected::Address => write!(
                f,
                "{var} is `{value}`, which is not an IP address; expected one such as \
                 `0.0.0.0`, `127.0.0.1`, `::` or `::1`, without a port"
            ),
            Expected::Port => write!(
                f,
                "{var} is `{value}`, which is not a port; expected a number from 1 to 65535"
            ),
        }
    }
}

impl std::error::Error for InvalidListenSetting {}

fn read(address: Var, port: Var, default: SocketAddr) -> Result<SocketAddr, InvalidListenSetting> {
    Ok(SocketAddr::new(
        parse_address(address, address.get().ok().as_deref(), default.ip())?,
        parse_port(port, port.get().ok().as_deref(), default.port())?,
    ))
}

/// `value` with surrounding whitespace removed, or `None` when that leaves
/// nothing. Unset and blank both mean the default, so a compose file can pass
/// a variable through without a value of its own.
fn given(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// `value` as `var`'s address, `default` when it gives none.
fn parse_address(
    var: Var,
    value: Option<&str>,
    default: IpAddr,
) -> Result<IpAddr, InvalidListenSetting> {
    let Some(value) = given(value) else {
        return Ok(default);
    };
    value.parse().map_err(|_| InvalidListenSetting {
        var,
        value: value.to_string(),
        expected: Expected::Address,
    })
}

/// `value` as `var`'s port, `default` when it gives none.
///
/// Port 0 is refused: it asks the system for any free port, which no client
/// could then be told about.
fn parse_port(var: Var, value: Option<&str>, default: u16) -> Result<u16, InvalidListenSetting> {
    let Some(value) = given(value) else {
        return Ok(default);
    };
    match value.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(InvalidListenSetting {
            var,
            value: value.to_string(),
            expected: Expected::Port,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_LISTEN_ADDRESS, Expected, InvalidListenSetting, ListenAddresses, parse_address,
        parse_port,
    };
    use aurcache_common::server_env::{AURCACHE_HTTP_LISTEN_ADDRESS, AURCACHE_REPO_PORT};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    fn address(value: &str) -> IpAddr {
        parse_address(
            AURCACHE_HTTP_LISTEN_ADDRESS,
            Some(value),
            DEFAULT_LISTEN_ADDRESS,
        )
        .expect("an address")
    }

    fn port(value: &str) -> u16 {
        parse_port(AURCACHE_REPO_PORT, Some(value), 8081).expect("a port")
    }

    #[test]
    fn the_defaults_are_every_ipv4_interface_on_the_usual_ports() {
        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(DEFAULT_LISTEN_ADDRESS, any);
        assert_eq!(
            ListenAddresses::default(),
            ListenAddresses {
                api: SocketAddr::new(any, 8080),
                repo: SocketAddr::new(any, 8081),
                worker: SocketAddr::new(any, 8083),
            }
        );
    }

    #[test]
    fn unset_or_blank_is_the_default() {
        let default = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(
            parse_address(AURCACHE_HTTP_LISTEN_ADDRESS, None, default),
            Ok(default)
        );
        assert_eq!(
            parse_address(AURCACHE_HTTP_LISTEN_ADDRESS, Some(" "), default),
            Ok(default)
        );
        assert_eq!(parse_port(AURCACHE_REPO_PORT, None, 9000), Ok(9000));
        assert_eq!(parse_port(AURCACHE_REPO_PORT, Some(""), 9000), Ok(9000));
    }

    #[test]
    fn takes_ipv4() {
        assert_eq!(address("127.0.0.1"), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(
            address(" 192.168.1.20 "),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))
        );
    }

    #[test]
    fn takes_ipv6() {
        assert_eq!(address("::"), IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert_eq!(address("::1"), IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(
            address("fd00::20"),
            IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x20))
        );
    }

    #[test]
    fn takes_a_port() {
        assert_eq!(port("1"), 1);
        assert_eq!(port(" 9081 "), 9081);
        assert_eq!(port("65535"), 65535);
    }

    /// A host name, a port or brackets are refused rather than ignored, and
    /// the refusal names the variable so the operator knows which to fix.
    #[test]
    fn refuses_an_address_that_is_not_one_naming_the_variable() {
        for value in ["localhost", "0.0.0.0:8080", "[::1]", "300.1.1.1", "eth0"] {
            let err = parse_address(
                AURCACHE_HTTP_LISTEN_ADDRESS,
                Some(value),
                DEFAULT_LISTEN_ADDRESS,
            )
            .unwrap_err();
            assert_eq!(
                err,
                InvalidListenSetting {
                    var: AURCACHE_HTTP_LISTEN_ADDRESS,
                    value: value.to_string(),
                    expected: Expected::Address,
                }
            );
            let message = err.to_string();
            assert!(
                message.starts_with("AURCACHE_HTTP_LISTEN_ADDRESS is `"),
                "{message}"
            );
            assert!(message.contains("not an IP address"), "{message}");
        }
    }

    /// Port 0 would bind somewhere no client could be told about.
    #[test]
    fn refuses_a_port_that_is_not_one_naming_the_variable() {
        for value in ["0", "65536", "-1", "80a", "8081.0", ":8081"] {
            let err = parse_port(AURCACHE_REPO_PORT, Some(value), 8081).unwrap_err();
            assert_eq!(
                err,
                InvalidListenSetting {
                    var: AURCACHE_REPO_PORT,
                    value: value.to_string(),
                    expected: Expected::Port,
                }
            );
            let message = err.to_string();
            assert!(message.starts_with("AURCACHE_REPO_PORT is `"), "{message}");
            assert!(message.contains("not a port"), "{message}");
        }
    }
}
