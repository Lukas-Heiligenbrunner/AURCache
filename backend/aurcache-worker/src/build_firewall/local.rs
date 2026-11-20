//! What counts as a local destination.
//!
//! Two kinds: fixed ranges that are local wherever the worker is (loopback,
//! the private ranges, link-local, multicast, and their NAT64 forms), and the
//! worker host's own links, read from its routing table as each build starts:
//! the prefixes it reaches without a router, and its default gateways. The
//! second kind is what makes a LAN on public IPv4 addresses, or the global
//! IPv6 addresses of a dual-stack one, local.
//!
//! A destination beyond a gateway is not on a link: the internet stays
//! reachable through the default route, and the default route itself names no
//! prefix.

use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::Command;

use super::Family;

/// An address and how many of its leading bits a destination shares with it.
///
/// The address is the prefix's first one: the bits past `len` are zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Prefix {
    pub addr: IpAddr,
    pub len: u8,
}

/// The NAT64 well-known prefix, `64:ff9b::/96`: an IPv4 address in its last
/// 32 bits is that address reached through a translator.
const NAT64_WELL_KNOWN: Ipv6Addr = Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0);

impl Prefix {
    const fn v4(octets: [u8; 4], len: u8) -> Self {
        let [a, b, c, d] = octets;
        Self {
            addr: IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
            len,
        }
    }

    const fn v6(segments: [u16; 8], len: u8) -> Self {
        let [a, b, c, d, e, f, g, h] = segments;
        Self {
            addr: IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h)),
            len,
        }
    }

    /// The prefix of `len` bits `addr` is in; `None` for a length past the
    /// address's.
    #[must_use]
    pub fn new(addr: IpAddr, len: u8) -> Option<Self> {
        let width = Family::of(&addr).bits();
        (len <= width).then(|| Self {
            addr: from_bits(Family::of(&addr), bits(&addr) & mask(len, width)),
            len,
        })
    }

    /// The prefix holding `addr` alone.
    #[must_use]
    pub const fn host(addr: IpAddr) -> Self {
        let len = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        Self { addr, len }
    }

    /// `address/len`, or an address alone for the prefix holding just it.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.split_once('/') {
            Some((addr, len)) => Self::new(addr.parse().ok()?, len.parse().ok()?),
            None => Some(Self::host(text.parse().ok()?)),
        }
    }

    #[must_use]
    pub const fn family(&self) -> Family {
        Family::of(&self.addr)
    }

    /// Whether every address in `other` is in this prefix.
    #[must_use]
    pub fn contains(&self, other: &Self) -> bool {
        let family = self.family();
        family == other.family()
            && self.len <= other.len
            && bits(&other.addr) & mask(self.len, family.bits()) == bits(&self.addr)
    }

    /// The same addresses reached through a NAT64 translator on the
    /// well-known prefix; `None` for an IPv6 prefix.
    #[must_use]
    pub fn nat64(&self) -> Option<Self> {
        let IpAddr::V4(v4) = self.addr else {
            return None;
        };
        let addr = u128::from(NAT64_WELL_KNOWN) | u128::from(u32::from(v4));
        Some(Self {
            addr: IpAddr::V6(Ipv6Addr::from(addr)),
            len: self.len + 96,
        })
    }
}

impl std::fmt::Display for Prefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.len == self.family().bits() {
            write!(f, "{}", self.addr)
        } else {
            write!(f, "{}/{}", self.addr, self.len)
        }
    }
}

fn bits(addr: &IpAddr) -> u128 {
    match addr {
        IpAddr::V4(a) => u128::from(u32::from(*a)),
        IpAddr::V6(a) => u128::from(*a),
    }
}

/// `len` leading ones in an address of `width` bits.
fn mask(len: u8, width: u8) -> u128 {
    if len == 0 {
        return 0;
    }
    let all = if width == 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    };
    all & !((1u128 << (width - len)) - 1)
}

fn from_bits(family: Family, bits: u128) -> IpAddr {
    match family {
        #[allow(clippy::cast_possible_truncation)]
        Family::V4 => IpAddr::V4(Ipv4Addr::from(bits as u32)),
        Family::V6 => IpAddr::V6(Ipv6Addr::from(bits)),
    }
}

/// Local wherever the worker is, IPv4: this network, the private ranges,
/// carrier-grade NAT's shared range, loopback, link-local, the benchmarking
/// range, multicast, and the reserved range, the limited broadcast address
/// with it.
pub const LOCAL_V4: [Prefix; 10] = [
    Prefix::v4([0, 0, 0, 0], 8),
    Prefix::v4([10, 0, 0, 0], 8),
    Prefix::v4([100, 64, 0, 0], 10),
    Prefix::v4([127, 0, 0, 0], 8),
    Prefix::v4([169, 254, 0, 0], 16),
    Prefix::v4([172, 16, 0, 0], 12),
    Prefix::v4([192, 168, 0, 0], 16),
    Prefix::v4([198, 18, 0, 0], 15),
    Prefix::v4([224, 0, 0, 0], 4),
    Prefix::v4([240, 0, 0, 0], 4),
];

/// Local wherever the worker is, IPv6, besides the NAT64 forms of
/// [`LOCAL_V4`]: loopback, unique local, link-local, multicast, and the
/// local-use NAT64 prefix. What a translator on that prefix reaches is the
/// network's own choice, and which IPv4 address one of its addresses embeds
/// depends on a prefix length the worker cannot know, so all of it is local.
const LOCAL_V6: [Prefix; 5] = [
    Prefix::v6([0, 0, 0, 0, 0, 0, 0, 1], 128),
    Prefix::v6([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),
    Prefix::v6([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),
    Prefix::v6([0xff00, 0, 0, 0, 0, 0, 0, 0], 8),
    Prefix::v6([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48),
];

/// The destinations of `family` that are local wherever the worker is.
#[must_use]
pub fn fixed(family: Family) -> Vec<Prefix> {
    match family {
        Family::V4 => LOCAL_V4.to_vec(),
        Family::V6 => LOCAL_V6
            .iter()
            .copied()
            .chain(LOCAL_V4.iter().filter_map(Prefix::nat64))
            .collect(),
    }
}

/// The worker host's own links, as its main routing table has them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Links {
    /// What it reaches without a router: the prefix of each address on its
    /// interfaces, and what routers advertise as on-link.
    pub on_link: Vec<Prefix>,
    /// The routers its default routes go through.
    pub gateways: Vec<IpAddr>,
}

impl Links {
    /// Read the links from `ip -j route`, both families. Blocking.
    ///
    /// # Errors
    /// When `ip` cannot be run or its listing read: the build must not run
    /// with its local network missing from what it is refused.
    pub fn read() -> Result<Self> {
        let mut links = routes(&listing(Family::V4)?);
        // A kernel without IPv6 has no IPv6 routes, and `ip -6` fails there.
        if Path::new("/proc/net/if_inet6").exists() {
            let v6 = routes(&listing(Family::V6)?);
            links.on_link.extend(v6.on_link);
            links.gateways.extend(v6.gateways);
        }
        Ok(links)
    }

    /// The destinations of `family` these links make local: the on-link
    /// prefixes and the gateways, and for IPv6 also the NAT64 forms of the
    /// IPv4 ones. Without overlaps, as an nftables interval set takes them.
    #[must_use]
    pub fn local(&self, family: Family) -> Vec<Prefix> {
        let own: Vec<Prefix> = self
            .on_link
            .iter()
            .copied()
            .chain(self.gateways.iter().copied().map(Prefix::host))
            .collect();
        let translated = own.iter().filter_map(Prefix::nat64).collect::<Vec<_>>();
        disjoint(
            own.into_iter()
                .chain(translated)
                .filter(|p| p.family() == family)
                .collect(),
        )
    }
}

/// `ip -j route show table main` for one family.
fn listing(family: Family) -> Result<serde_json::Value> {
    let flag = match family {
        Family::V4 => "-4",
        Family::V6 => "-6",
    };
    let output = Command::new("ip")
        .args(["-j", flag, "route", "show", "table", "main"])
        .output()
        .context("running ip to read the worker's routes")?;
    anyhow::ensure!(
        output.status.success(),
        "ip {flag} route failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("parsing ip's route listing")
}

/// Read [`Links`] from one `ip -j route` listing.
///
/// On-link is a unicast route with no gateway that the kernel made for an
/// address on an interface (`kernel`) or that a router advertised (`ra`).
/// Routes added by other means, a VPN's or a static one, are left out: a
/// tunnel that carries the internet is routed the same way.
#[must_use]
pub fn routes(listing: &serde_json::Value) -> Links {
    let mut links = Links::default();
    let routes = listing.as_array().map(Vec::as_slice).unwrap_or_default();
    for route in routes {
        let field = |name: &str| route.get(name).and_then(serde_json::Value::as_str);
        if field("type").is_some_and(|t| t != "unicast") {
            continue;
        }
        let nexthops = route
            .get("nexthops")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let gateways = field("gateway")
            .into_iter()
            .chain(
                nexthops
                    .iter()
                    .filter_map(|hop| hop.get("gateway")?.as_str()),
            )
            .filter_map(|gateway| gateway.parse::<IpAddr>().ok());
        if field("dst") == Some("default") {
            links.gateways.extend(gateways);
            continue;
        }
        if gateways.count() > 0 || !matches!(field("protocol"), Some("kernel" | "ra")) {
            continue;
        }
        if let Some(prefix) = field("dst").and_then(Prefix::parse)
            && prefix.len > 0
        {
            links.on_link.push(prefix);
        }
    }
    links
}

/// The prefixes no other one in `prefixes` contains, sorted.
///
/// Two prefixes either nest or do not meet, so this leaves no overlap.
#[must_use]
pub fn disjoint(mut prefixes: Vec<Prefix>) -> Vec<Prefix> {
    prefixes.sort_unstable();
    prefixes.dedup();
    let mut kept: Vec<Prefix> = Vec::with_capacity(prefixes.len());
    for prefix in prefixes {
        if !kept.last().is_some_and(|last| last.contains(&prefix)) {
            kept.push(prefix);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Prefix {
        Prefix::parse(text).unwrap()
    }

    fn shown(prefixes: &[Prefix]) -> Vec<String> {
        prefixes.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_prefix_is_its_first_address_and_its_length() {
        assert_eq!(p("192.168.1.77/24").to_string(), "192.168.1.0/24");
        assert_eq!(p("2001:db8:1::5/64").to_string(), "2001:db8:1::/64");
        assert_eq!(p("10.9.0.1").to_string(), "10.9.0.1");
        assert_eq!(p("0.0.0.0/0").to_string(), "0.0.0.0/0");
        assert_eq!(Prefix::parse("10.0.0.0/33"), None);
        assert_eq!(Prefix::parse("default"), None);
    }

    #[test]
    fn containment_is_by_family_and_leading_bits() {
        assert!(p("10.0.0.0/8").contains(&p("10.9.0.0/22")));
        assert!(p("10.9.0.0/22").contains(&p("10.9.2.104")));
        assert!(!p("10.9.0.0/22").contains(&p("10.9.4.0")));
        assert!(!p("10.9.0.0/22").contains(&p("10.0.0.0/8")));
        assert!(!p("::/0").contains(&p("10.0.0.0/8")));
    }

    #[test]
    fn an_ipv4_prefix_has_a_nat64_form() {
        assert_eq!(
            p("10.0.0.0/8").nat64().unwrap().to_string(),
            "64:ff9b::a00:0/104"
        );
        assert_eq!(
            p("192.168.0.0/16").nat64().unwrap().to_string(),
            "64:ff9b::c0a8:0/112"
        );
        assert_eq!(
            p("203.0.113.1").nat64().unwrap().to_string(),
            "64:ff9b::cb00:7101"
        );
        assert_eq!(p("fc00::/7").nat64(), None);
    }

    /// nftables refuses overlapping elements in a set.
    #[test]
    fn the_fixed_ranges_do_not_overlap() {
        for family in Family::ALL {
            let ranges = fixed(family);
            assert_eq!(disjoint(ranges.clone()).len(), ranges.len(), "{ranges:#?}");
            assert!(ranges.iter().all(|r| r.family() == family));
        }
        let v6 = shown(&fixed(Family::V6));
        for nat64 in [
            "64:ff9b::a00:0/104",
            "64:ff9b::7f00:0/104",
            "64:ff9b::f000:0/100",
        ] {
            assert!(v6.iter().any(|r| r == nat64), "{v6:#?}");
        }
        assert!(v6.iter().any(|r| r == "64:ff9b:1::/48"));
    }

    /// What a dual-stack host with Docker on it lists, trimmed: a LAN on
    /// public IPv4, an RA prefix, a bridge, a VPN's route, a route a router
    /// advertised through itself, and the default routes.
    fn host_routes() -> serde_json::Value {
        serde_json::json!([
            {"dst": "default", "gateway": "203.0.113.1", "dev": "eth0", "protocol": "dhcp", "metric": 100, "flags": []},
            {"dst": "203.0.113.0/24", "dev": "eth0", "protocol": "kernel", "scope": "link", "prefsrc": "203.0.113.20", "flags": []},
            {"dst": "172.17.0.0/16", "dev": "docker0", "protocol": "kernel", "scope": "link", "prefsrc": "172.17.0.1", "flags": ["linkdown"]},
            {"dst": "0.0.0.0/1", "dev": "wg0", "scope": "link", "flags": []},
            {"dst": "198.51.100.0/24", "gateway": "203.0.113.5", "dev": "eth0", "protocol": "static", "flags": []},
            {"type": "unreachable", "dst": "192.0.2.0/24", "protocol": "static", "flags": []},
            {"dst": "2001:db8:1::/64", "dev": "eth0", "protocol": "ra", "metric": 100, "flags": [], "pref": "medium"},
            {"dst": "fe80::/64", "dev": "eth0", "protocol": "kernel", "metric": 1024, "flags": [], "pref": "medium"},
            {"dst": "fe80::/64", "dev": "docker0", "protocol": "kernel", "metric": 256, "flags": [], "pref": "medium"},
            {"dst": "2001:db8:2::/64", "protocol": "ra", "metric": 105, "flags": [], "pref": "medium",
                "nexthops": [{"gateway": "fe80::1", "dev": "eth0", "weight": 1, "flags": []}]},
            {"dst": "default", "protocol": "ra", "metric": 1024, "flags": [], "pref": "medium",
                "nexthops": [{"gateway": "fe80::1", "dev": "eth0", "weight": 1, "flags": []},
                             {"gateway": "fe80::2", "dev": "eth0", "weight": 1, "flags": []}]},
        ])
    }

    #[test]
    fn on_link_prefixes_and_default_gateways_are_read_from_the_routes() {
        let links = routes(&host_routes());
        assert_eq!(
            shown(&links.on_link),
            [
                "203.0.113.0/24",
                "172.17.0.0/16",
                "2001:db8:1::/64",
                "fe80::/64",
                "fe80::/64"
            ]
        );
        assert_eq!(
            links
                .gateways
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["203.0.113.1", "fe80::1", "fe80::2"]
        );
    }

    /// The internet stays reachable through the gateway: no default route,
    /// tunnel or routed prefix becomes local.
    #[test]
    fn nothing_beyond_a_gateway_is_local() {
        let links = routes(&host_routes());
        let local: Vec<Prefix> = Family::ALL
            .into_iter()
            .flat_map(|family| links.local(family))
            .collect();
        for beyond in [
            "8.8.8.8",
            "198.51.100.7",
            "128.0.0.1",
            "2001:db8:2::7",
            "2606:4700::1111",
        ] {
            assert!(
                !local.iter().any(|l| l.contains(&p(beyond))),
                "{beyond} is local: {local:#?}"
            );
        }
    }

    #[test]
    fn the_links_local_set_has_gateways_and_nat64_forms_without_overlap() {
        let links = routes(&host_routes());
        assert_eq!(
            shown(&links.local(Family::V4)),
            ["172.17.0.0/16", "203.0.113.0/24"]
        );
        assert_eq!(
            shown(&links.local(Family::V6)),
            [
                "64:ff9b::ac11:0/112",
                "64:ff9b::cb00:7100/120",
                "2001:db8:1::/64",
                "fe80::/64"
            ]
        );
        // A gateway off every on-link prefix is there by itself.
        let links = Links {
            on_link: Vec::new(),
            gateways: vec!["192.0.2.1".parse().unwrap()],
        };
        assert_eq!(shown(&links.local(Family::V4)), ["192.0.2.1"]);
    }

    #[test]
    fn nested_prefixes_keep_only_the_outer_one() {
        assert_eq!(
            shown(&disjoint(vec![
                p("10.9.2.104"),
                p("10.0.0.0/8"),
                p("10.9.0.0/22"),
                p("11.0.0.0/8"),
                p("10.0.0.0/8"),
            ])),
            ["10.0.0.0/8", "11.0.0.0/8"]
        );
    }
}
