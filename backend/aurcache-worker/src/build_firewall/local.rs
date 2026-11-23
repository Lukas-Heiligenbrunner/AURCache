//! What counts as a local destination: fixed ranges local wherever the
//! worker is, and the prefix of every address on its interfaces, which covers
//! a LAN on public IPv4 or global IPv6 addresses. The internet is beyond a
//! router, in none of those prefixes.

use anyhow::{Context, Result};
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use nix::ifaddrs::getifaddrs;
use nix::sys::socket::SockaddrStorage;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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
/// the reserved range, the limited broadcast address with it.
const LOCAL_V4: [IpNet; 9] = [
    v4(0, 0, 8),
    v4(10, 0, 8),
    v4(100, 64, 10),
    v4(127, 0, 8),
    v4(169, 254, 16),
    v4(172, 16, 12),
    v4(192, 168, 16),
    v4(224, 0, 4),
    v4(240, 0, 4),
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

/// The prefix of every address on the worker host's interfaces, however the
/// address or its route was made (DHCP, `noprefixroute`, a VRF), and of each
/// address's peer. Blocking.
///
/// # Errors
/// When `getifaddrs` fails, or finds no address: loopback always has one.
pub fn links() -> Result<Vec<IpNet>> {
    let mut links = Vec::new();
    for link in getifaddrs().context("reading the addresses of the host's interfaces")? {
        let netmask = link.netmask.as_ref().and_then(ip);
        // One field holds an address's peer, named after the link's flags:
        // `destination` on a point-to-point link, `broadcast` on any other,
        // where a broadcast address is in the prefix already.
        for addr in [link.address, link.destination, link.broadcast] {
            links.extend(addr.as_ref().and_then(ip).and_then(|a| prefix(a, netmask)));
        }
    }
    anyhow::ensure!(!links.is_empty(), "the host's interfaces have no address");
    Ok(links)
}

/// The prefix of `addr` under `netmask`, truncated, as older nft releases
/// refuse host bits; `addr` alone without a netmask. `None` for a /0, which
/// would make the whole internet local.
pub fn prefix(addr: IpAddr, netmask: Option<IpAddr>) -> Option<IpNet> {
    let net = match netmask {
        Some(mask) => IpNet::with_netmask(addr, mask).ok()?,
        None => IpNet::from(addr),
    };
    (net.prefix_len() > 0).then(|| net.trunc())
}

fn ip(addr: &SockaddrStorage) -> Option<IpAddr> {
    let v4 = addr.as_sockaddr_in().map(|a| IpAddr::V4(a.ip()));
    v4.or_else(|| addr.as_sockaddr_in6().map(|a| IpAddr::V6(a.ip())))
}
