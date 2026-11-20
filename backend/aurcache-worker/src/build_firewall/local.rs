//! What counts as a local destination wherever the worker is: loopback, the
//! private ranges, carrier-grade NAT's shared range, link-local and
//! multicast.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::Family;

/// An address and how many of its leading bits a destination shares with it.
///
/// The address is the prefix's first one: the bits past `len` are zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Prefix {
    pub addr: IpAddr,
    pub len: u8,
}

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

    /// The prefix holding `addr` alone.
    #[must_use]
    pub const fn host(addr: IpAddr) -> Self {
        let len = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        Self { addr, len }
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

/// Local wherever the worker is, IPv4: this network, the private ranges,
/// carrier-grade NAT's shared range, loopback, link-local, multicast, and
/// the limited broadcast address.
const LOCAL_V4: [Prefix; 9] = [
    Prefix::v4([0, 0, 0, 0], 8),
    Prefix::v4([10, 0, 0, 0], 8),
    Prefix::v4([100, 64, 0, 0], 10),
    Prefix::v4([127, 0, 0, 0], 8),
    Prefix::v4([169, 254, 0, 0], 16),
    Prefix::v4([172, 16, 0, 0], 12),
    Prefix::v4([192, 168, 0, 0], 16),
    Prefix::v4([224, 0, 0, 0], 4),
    Prefix::v4([255, 255, 255, 255], 32),
];

/// Local wherever the worker is, IPv6: loopback, unique local, link-local and
/// multicast.
const LOCAL_V6: [Prefix; 4] = [
    Prefix::v6([0, 0, 0, 0, 0, 0, 0, 1], 128),
    Prefix::v6([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),
    Prefix::v6([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),
    Prefix::v6([0xff00, 0, 0, 0, 0, 0, 0, 0], 8),
];

/// The destinations of `family` that are local wherever the worker is.
#[must_use]
pub fn fixed(family: Family) -> Vec<Prefix> {
    match family {
        Family::V4 => LOCAL_V4.to_vec(),
        Family::V6 => LOCAL_V6.to_vec(),
    }
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

    /// `address/len`, or an address alone; the address already the prefix's
    /// first one.
    fn p(text: &str) -> Prefix {
        match text.split_once('/') {
            Some((addr, len)) => Prefix {
                addr: addr.parse().unwrap(),
                len: len.parse().unwrap(),
            },
            None => Prefix::host(text.parse().unwrap()),
        }
    }

    fn shown(prefixes: &[Prefix]) -> Vec<String> {
        prefixes.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn containment_is_by_family_and_leading_bits() {
        assert!(p("10.0.0.0/8").contains(&p("10.9.0.0/22")));
        assert!(p("10.9.0.0/22").contains(&p("10.9.2.104")));
        assert!(!p("10.9.0.0/22").contains(&p("10.9.4.0")));
        assert!(!p("10.9.0.0/22").contains(&p("10.0.0.0/8")));
        assert!(!p("::/0").contains(&p("10.0.0.0/8")));
    }

    /// nftables refuses overlapping elements in a set.
    #[test]
    fn the_fixed_ranges_do_not_overlap() {
        for family in Family::ALL {
            let ranges = fixed(family);
            assert_eq!(disjoint(ranges.clone()).len(), ranges.len(), "{ranges:#?}");
            assert!(ranges.iter().all(|r| r.family() == family));
        }
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
