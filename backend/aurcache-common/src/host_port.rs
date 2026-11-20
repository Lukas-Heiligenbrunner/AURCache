//! `host:port` entries naming what a build may reach on the local network.
//!
//! The worker's `WORKER_BUILD_ALLOW` is a list of them, and so is the server's
//! `build_hosts` setting. One parser, so an entry the server accepts is one
//! every worker reads the same way. An entry opens its port on that host to
//! TCP and to UDP alike.
//!
//! The host is a name or an address; an IPv6 address goes in brackets, as in a
//! URL (`[fd00::5]:3632`), and an IPv4 address is written in full, in decimal:
//! the C library reads `127.1`, `0x7f.1` and `0177.0.0.1` as 127.0.0.1, so
//! those are refused rather than taken for names. A link-local IPv6 address with a zone
//! (`[fe80::1%eth0]:22`) is refused: the rules match an address, not the
//! interface a zone names. Entries in a list are separated by commas or
//! whitespace.

use std::fmt;

/// One destination: a host and a port on it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostPort {
    /// A name, or an address -- an IPv6 one without its brackets.
    pub host: String,
    pub port: u16,
}

/// Why an entry is not a `host:port`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPortError {
    /// No `:port` at the end.
    MissingPort,
    /// The port is not a number from 1 to 65535.
    BadPort,
    /// Nothing before the port.
    MissingHost,
    /// The host has characters no name or address has, or an IPv6 address
    /// without its brackets.
    BadHost,
}

impl fmt::Display for HostPortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingPort => "no port: write it as host:port",
            Self::BadPort => "the port must be a number from 1 to 65535",
            Self::MissingHost => "no host before the port",
            Self::BadHost => {
                "the host must be a name or an address, with an IPv6 address in brackets"
            }
        })
    }
}

impl std::error::Error for HostPortError {}

/// An entry of a list that did not parse, with what was wrong with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidEntry {
    pub entry: String,
    pub error: HostPortError,
}

impl fmt::Display for InvalidEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.entry, self.error)
    }
}

/// A list split into what parsed and what did not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostPortList {
    pub entries: Vec<HostPort>,
    pub invalid: Vec<InvalidEntry>,
}

impl HostPort {
    /// Parse one `host:port` entry.
    ///
    /// # Errors
    /// What is wrong with it, when it is not one.
    pub fn parse(raw: &str) -> Result<Self, HostPortError> {
        let raw = raw.trim();
        let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
            let (host, after) = rest.split_once(']').ok_or(HostPortError::BadHost)?;
            let port = after.strip_prefix(':').ok_or(HostPortError::MissingPort)?;
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(HostPortError::BadHost);
            }
            (host, port)
        } else {
            let (host, port) = raw.rsplit_once(':').ok_or(HostPortError::MissingPort)?;
            // An unbracketed IPv6 address would split at its last group.
            if host.contains(':') {
                return Err(HostPortError::BadHost);
            }
            if !host.chars().all(is_name_char) {
                return Err(HostPortError::BadHost);
            }
            // Numbers alone are an IPv4 address in full, or refused: the C
            // library's resolver reads `1.2.3` as 1.2.0.3 and `0x7f.1` as
            // 127.0.0.1, an address the entry would not show.
            if !host.is_empty() && is_numeric(host) && host.parse::<std::net::Ipv4Addr>().is_err() {
                return Err(HostPortError::BadHost);
            }
            (host, port)
        };
        if host.is_empty() {
            return Err(HostPortError::MissingHost);
        }
        // Digits only: `u16`'s parser takes a leading `+` as well.
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(HostPortError::BadPort);
        }
        let port = match port.parse::<u16>() {
            Ok(0) | Err(_) => return Err(HostPortError::BadPort),
            Ok(port) => port,
        };
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// Parse a list of entries separated by commas or whitespace, keeping
    /// apart the ones that do not parse so the caller can say which.
    #[must_use]
    pub fn parse_list(raw: &str) -> HostPortList {
        let mut list = HostPortList::default();
        for entry in raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|e| !e.is_empty())
        {
            match Self::parse(entry) {
                Ok(parsed) => {
                    if !list.entries.contains(&parsed) {
                        list.entries.push(parsed);
                    }
                }
                Err(error) => list.invalid.push(InvalidEntry {
                    entry: entry.to_string(),
                    error,
                }),
            }
        }
        list
    }
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// Whether every dot-separated part of `host` is a number as `inet_aton`
/// reads one: decimal, octal with a leading `0`, or hexadecimal after `0x`.
fn is_numeric(host: &str) -> bool {
    host.split('.').all(
        |part| match part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
            Some(hex) => hex.chars().all(|c| c.is_ascii_hexdigit()),
            None => part.chars().all(|c| c.is_ascii_digit()),
        },
    )
}

/// What a DNS name or an IPv4 address is made of.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(host: &str, port: u16) -> HostPort {
        HostPort {
            host: host.to_string(),
            port,
        }
    }

    #[test]
    fn names_and_addresses_parse() {
        assert_eq!(
            HostPort::parse("distcc.lan:3632"),
            Ok(at("distcc.lan", 3632))
        );
        assert_eq!(
            HostPort::parse(" 192.168.1.5:22 "),
            Ok(at("192.168.1.5", 22))
        );
        assert_eq!(HostPort::parse("[fd00::5]:3632"), Ok(at("fd00::5", 3632)));
        // A name may be made of hex digits, or start with a number.
        assert_eq!(HostPort::parse("deadbeef:22"), Ok(at("deadbeef", 22)));
        assert_eq!(HostPort::parse("0x7f.lan:22"), Ok(at("0x7f.lan", 22)));
        assert_eq!(HostPort::parse("10.lan:22"), Ok(at("10.lan", 22)));
        // Names are case-insensitive, and compared lowercased.
        assert_eq!(HostPort::parse("GitLab.LAN:22"), Ok(at("gitlab.lan", 22)));
    }

    #[test]
    fn what_is_wrong_is_named() {
        assert_eq!(
            HostPort::parse("distcc.lan"),
            Err(HostPortError::MissingPort)
        );
        assert_eq!(HostPort::parse(":22"), Err(HostPortError::MissingHost));
        assert_eq!(HostPort::parse("host:0"), Err(HostPortError::BadPort));
        assert_eq!(HostPort::parse("host:65536"), Err(HostPortError::BadPort));
        assert_eq!(HostPort::parse("host:ssh"), Err(HostPortError::BadPort));
        assert_eq!(HostPort::parse("fd00::5:22"), Err(HostPortError::BadHost));
        assert_eq!(
            HostPort::parse("[fd00::5]"),
            Err(HostPortError::MissingPort)
        );
        assert_eq!(HostPort::parse("[not-v6]:22"), Err(HostPortError::BadHost));
        assert_eq!(HostPort::parse("http://x:80"), Err(HostPortError::BadHost));
        assert_eq!(HostPort::parse("a b:80"), Err(HostPortError::BadHost));
        assert_eq!(HostPort::parse("host:+22"), Err(HostPortError::BadPort));
        assert_eq!(HostPort::parse("1.2.3:22"), Err(HostPortError::BadHost));
        // The forms the C library reads as an address, 127.0.0.1 here.
        for numeric in [
            "127.1",
            "2130706433",
            "0x7f.1",
            "0X7F.0.0.1",
            "0x7f000001",
            "0177.0.0.1",
            "127.0.0.01",
        ] {
            assert_eq!(
                HostPort::parse(&format!("{numeric}:22")),
                Err(HostPortError::BadHost),
                "{numeric}"
            );
        }
        assert_eq!(
            HostPort::parse("10.0.0.256:22"),
            Err(HostPortError::BadHost)
        );
        assert_eq!(
            HostPort::parse("[fe80::1%eth0]:22"),
            Err(HostPortError::BadHost)
        );
    }

    #[test]
    fn a_list_keeps_what_parsed_and_names_the_rest() {
        let list = HostPort::parse_list("distcc.lan:3632, flexo.lan:7878\n[fd00::5]:22,,nope");
        assert_eq!(
            list.entries,
            [
                at("distcc.lan", 3632),
                at("flexo.lan", 7878),
                at("fd00::5", 22)
            ]
        );
        assert_eq!(
            list.invalid,
            [InvalidEntry {
                entry: "nope".to_string(),
                error: HostPortError::MissingPort,
            }]
        );
        // The same entry twice is one entry.
        assert_eq!(HostPort::parse_list("a:1 a:1").entries.len(), 1);
    }

    #[test]
    fn display_round_trips() {
        for raw in ["distcc.lan:3632", "[fd00::5]:22", "10.0.0.2:8081"] {
            assert_eq!(HostPort::parse(raw).unwrap().to_string(), raw);
        }
    }
}
