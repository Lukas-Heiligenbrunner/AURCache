use std::net::Ipv4Addr;

/// Default address on which the AURCache HTTP API and file server listen.
pub const AURCACHE_HTTP_IP: Ipv4Addr = Ipv4Addr::UNSPECIFIED;
/// Default address on which the AURCache pacman repo / mirror file server listens.
pub const AURCACHE_MIRROR_IP: Ipv4Addr = Ipv4Addr::UNSPECIFIED;
/// Default address for the dedicated remote-worker protocol listener.
pub const AURCACHE_WORKER_IP: Ipv4Addr = Ipv4Addr::UNSPECIFIED;
