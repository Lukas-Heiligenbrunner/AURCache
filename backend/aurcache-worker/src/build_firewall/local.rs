//! What counts as a local destination wherever the worker is: loopback, the
//! private ranges, carrier-grade NAT's shared range, link-local and
//! multicast.

use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use std::net::{Ipv4Addr, Ipv6Addr};

use super::Family;

const fn v4(a: u8, b: u8, len: u8) -> IpNet {
    IpNet::V4(Ipv4Net::new_assert(Ipv4Addr::new(a, b, 0, 0), len))
}

const fn v6(first: u16, len: u8) -> IpNet {
    let addr = Ipv6Addr::new(first, 0, 0, 0, 0, 0, 0, 0);
    IpNet::V6(Ipv6Net::new_assert(addr, len))
}

/// Local wherever the worker is, IPv4: this network, the private ranges,
/// carrier-grade NAT's shared range, loopback, link-local, multicast, and
/// the limited broadcast address.
const LOCAL_V4: [IpNet; 9] = [
    v4(0, 0, 8),
    v4(10, 0, 8),
    v4(100, 64, 10),
    v4(127, 0, 8),
    v4(169, 254, 16),
    v4(172, 16, 12),
    v4(192, 168, 16),
    v4(224, 0, 4),
    IpNet::V4(Ipv4Net::new_assert(Ipv4Addr::BROADCAST, 32)),
];

/// Local wherever the worker is, IPv6: loopback, unique local, link-local and
/// multicast.
const LOCAL_V6: [IpNet; 4] = [
    IpNet::V6(Ipv6Net::new_assert(Ipv6Addr::LOCALHOST, 128)),
    v6(0xfc00, 7),
    v6(0xfe80, 10),
    v6(0xff00, 8),
];

/// The destinations of `family` that are local wherever the worker is.
#[must_use]
pub const fn fixed(family: Family) -> &'static [IpNet] {
    match family {
        Family::V4 => &LOCAL_V4,
        Family::V6 => &LOCAL_V6,
    }
}
