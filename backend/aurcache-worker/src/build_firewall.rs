//! What a build may connect to. The design, with the reasons for each rule,
//! is design/implemented/build-network.md.
//!
//! Builds share the worker's network namespace and run code from people other
//! than the operator, as the build user and as root in the build's container.
//! That code must stay off the worker's local network and the server, whose
//! worker protocol takes a rogue registration and whose API, without OAuth,
//! serves anyone.
//!
//! ## The policy
//!
//! For every connection a build makes, in this order:
//!
//! 1. established and related traffic: accepted;
//! 2. DNS to the resolvers in the worker's `resolv.conf`: accepted;
//! 3. the allowlist, exact `host:port` pairs (the `Server =` lines of the
//!    build's `pacman.conf`, `WORKER_BUILD_ALLOW`): accepted;
//! 4. local destinations ([`local`]: fixed ranges and the worker's links),
//!    every address of the worker host, and every address of the server's
//!    hosts: refused, except a listener of the same build;
//! 5. everything else: accepted.
//!
//! Names are resolved, and the host's links read, as each build starts. The
//! server's hosts are resolved, and the links read, at startup too, which
//! must succeed, and builds only add to what they make local, so a failed
//! lookup never reopens anything. Refusals are a reset or an ICMP error,
//! recorded for the build's log.
//!
//! ## Which processes are a build's
//!
//! Each build has a cgroup of its own, which nspawn keeps its container in,
//! so `socket cgroupv2` matches every socket it opens, root's included, and
//! none of the worker's. The patched `makechrootpkg` rules out what a socket
//! match cannot see: raw frames, and sockets that the kernel opens for a
//! mount. A worker that cannot match builds by cgroup takes none.
//!
//! ## Layout
//!
//! Each worker owns one table ([`Table`]), made afresh at startup with the
//! baseline (rules 1, 4 and 5), the local sets, and a chain that each build's
//! own rules (2 to 4) hook into. Those load in one transaction after the
//! build's cgroup exists and before it starts, and go when it ends.
//!
//! A build's packet to a local address carries a mark naming the build, and
//! the input hook accepts it only into a socket of the same build: its own
//! listeners. Marks are build ids, so workers of different servers need
//! network namespaces of their own.
//!
//! `WORKER_BUILD_FIREWALL=0` installs none of this, and removes the table.

mod local;

use anyhow::{Context, Result};
use aurcache_worker_core::repo::Endpoint;
use ipnet::IpNet;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, ToSocketAddrs};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};

use crate::cgroup::{BuildCgroup, CgroupName, Hierarchy};

/// Environment switch that disables the policy; see the module docs.
const DISABLE_ENV: &str = "WORKER_BUILD_FIREWALL";

/// A build's packet to a local address carries a mark ([`Table::mark`]). This
/// is its top byte, and the rest names the build. Only those packets are
/// marked, and only while they cross loopback.
const MARK_TAG: u32 = 0x4100_0000;
const MARK_MASK: u32 = 0xff00_0000;

/// How many refused destinations a build records. Past that, refusals still
/// happen; they are only no longer listed.
const REFUSALS_RECORDED: u32 = 1024;

/// Whether the operator disabled the policy.
#[must_use]
pub fn disabled() -> bool {
    matches!(
        std::env::var(DISABLE_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// The nftables table one worker owns.
///
/// Named after the worker, so that workers sharing a network namespace each
/// make, fill and remove only their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    name: String,
}

impl Table {
    /// The table of the worker known by `id`, its identity's fingerprint:
    /// `aurcache_worker_` and the first 16 hex digits of it.
    #[must_use]
    pub fn for_worker(id: &str) -> Self {
        let id: String = id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(16)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        Self {
            name: format!("aurcache_worker_{id}"),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The mark carried by a build's packets to a local address: the tag, and
    /// the build's id below it.
    #[must_use]
    pub const fn mark(&self, build_id: i32) -> u32 {
        #[allow(clippy::cast_sign_loss)]
        let id = build_id as u32;
        MARK_TAG | (id & !MARK_MASK)
    }
}

impl std::fmt::Display for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// An address family, as the rules handle each one apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub const ALL: [Self; 2] = [Self::V4, Self::V6];

    #[must_use]
    pub const fn of(addr: &IpAddr) -> Self {
        match addr {
            IpAddr::V4(_) => Self::V4,
            IpAddr::V6(_) => Self::V6,
        }
    }

    /// What nftables matches its header with: `ip` or `ip6`.
    const fn keyword(self) -> &'static str {
        match self {
            Self::V4 => "ip",
            Self::V6 => "ip6",
        }
    }

    /// Its name for `meta nfproto`.
    const fn nfproto(self) -> &'static str {
        match self {
            Self::V4 => "ipv4",
            Self::V6 => "ipv6",
        }
    }

    /// nftables' type for one of its addresses.
    const fn addr_type(self) -> &'static str {
        match self {
            Self::V4 => "ipv4_addr",
            Self::V6 => "ipv6_addr",
        }
    }

    /// The table's set of local destinations: the prefixes of the host's links
    /// and the addresses of the server's hosts.
    const fn local_set(self) -> &'static str {
        match self {
            Self::V4 => "local4",
            Self::V6 => "local6",
        }
    }
}

/// `a, b, c`, for an nftables set.
fn joined<T: std::fmt::Display>(items: impl IntoIterator<Item = T>) -> String {
    items
        .into_iter()
        .map(|item| item.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// What matches every socket of the processes in `cgroup` and the cgroups
/// below it.
fn matching(cgroup: &CgroupName) -> String {
    format!("socket cgroupv2 level {} \"{}\"", cgroup.level, cgroup.path)
}

/// What one build may and may not reach, every name resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// The worker's DNS resolvers.
    pub resolvers: Vec<IpAddr>,
    /// The allowlist.
    pub allowed: Vec<SocketAddr>,
}

/// The table, made afresh: the baseline every build gets, on the builds'
/// cgroup, and the local sets, empty. They merge what is added to them, which
/// may overlap what they hold.
#[must_use]
pub fn baseline_script(table: &Table, builds: &CgroupName) -> String {
    let builds = matching(builds);
    let mut sets = String::new();
    let mut refusals = String::new();
    for family in Family::ALL {
        let set = family.local_set();
        let _ = writeln!(
            sets,
            "\tset {set} {{\n\t\ttype {}; flags interval; auto-merge;\n\t}}",
            family.addr_type()
        );
        let ip = family.keyword();
        let _ = writeln!(
            refusals,
            "\t\t{ip} daddr {{ {} }} goto refuse\n\t\t{ip} daddr @{set} goto refuse",
            joined(local::fixed(family))
        );
    }
    format!(
        "add table inet {table}\n\
         delete table inet {table}\n\
         table inet {table} {{\n\
         {sets}\
         \tchain output {{\n\
         \t\ttype filter hook output priority filter; policy accept;\n\
         \t\t{builds} jump builds\n\
         \t}}\n\
         \tchain builds {{\n\
         \t\tct state established,related accept\n\
         \t\tjump each_build\n\
         \t\tfib daddr type local goto refuse\n\
         {refusals}\
         \t}}\n\
         \tchain each_build {{\n\
         \t}}\n\
         \tchain refuse {{\n\
         \t\tmeta l4proto tcp reject with tcp reset\n\
         \t\treject with icmpx admin-prohibited\n\
         \t}}\n\
         \tchain input {{\n\
         \t\ttype filter hook input priority filter; policy accept;\n\
         \t\tiif \"lo\" meta mark & {MARK_MASK:#010x} == {MARK_TAG:#010x} jump each_build_in\n\
         \t}}\n\
         \tchain each_build_in {{\n\
         \t}}\n\
         }}\n"
    )
}

/// Add the host's `links` and the `server`'s addresses to the local sets,
/// which every build shares. They are only added to, so a short reading never
/// reopens anything. Past the repository's own `address:port`, which the
/// allowlist passes first, every port of the server's hosts is refused.
fn local_script(table: &Table, links: &[IpNet], server: &[IpAddr]) -> String {
    let mut s = String::new();
    for family in Family::ALL {
        let set = family.local_set();
        let mut local: Vec<IpNet> = links
            .iter()
            .copied()
            .chain(server.iter().copied().map(IpNet::from))
            .filter(|net| Family::of(&net.addr()) == family)
            .collect();
        local.sort_unstable();
        local.dedup();
        if !local.is_empty() {
            let _ = writeln!(s, "add element inet {table} {set} {{ {} }}", joined(local));
        }
    }
    s
}

/// The names of one build's chains and sets.
struct Names {
    /// Its chain, and the comment on every rule elsewhere that leads to it.
    chain: String,
    refuse: String,
    refused4: String,
    refused6: String,
}

impl Names {
    fn of(build_id: i32) -> Self {
        let chain = format!("build-{build_id}");
        Self {
            refuse: format!("{chain}-refuse"),
            refused4: format!("{chain}-refused4"),
            refused6: format!("{chain}-refused6"),
            chain,
        }
    }

    /// The set that records the build's refusals of `family`.
    fn refused(&self, family: Family) -> &str {
        match family {
            Family::V4 => &self.refused4,
            Family::V6 => &self.refused6,
        }
    }
}

/// One family's view of a policy.
struct FamilyPolicy {
    family: Family,
    resolvers: Vec<String>,
    /// `address . port`.
    allowed: Vec<String>,
}

impl FamilyPolicy {
    fn of(policy: &Policy, family: Family) -> Self {
        Self {
            family,
            resolvers: policy
                .resolvers
                .iter()
                .filter(|a| Family::of(a) == family)
                .map(ToString::to_string)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            allowed: pairs(&policy.allowed, family),
        }
    }
}

/// `address . port` for each of one family's endpoints, deduplicated.
fn pairs(endpoints: &[SocketAddr], family: Family) -> Vec<String> {
    endpoints
        .iter()
        .filter(|a| Family::of(&a.ip()) == family)
        .map(|a| format!("{} . {}", a.ip(), a.port()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// One build's rules, loaded in one transaction, on the build's `cgroup`.
#[must_use]
pub fn build_script(table: &Table, build_id: i32, cgroup: &CgroupName, policy: &Policy) -> String {
    let n = Names::of(build_id);
    let mut s = String::new();
    let rule = |s: &mut String, chain: &str, body: &str| {
        let _ = writeln!(s, "add rule inet {table} {chain} {body}");
    };
    for family in Family::ALL {
        let _ = writeln!(
            s,
            "add set inet {table} {} {{ type {} . inet_proto . inet_service; \
             flags dynamic; size {REFUSALS_RECORDED}; }}",
            n.refused(family),
            family.addr_type()
        );
    }
    // A refusal is recorded in a rule of its own: a full set stops the rule
    // that adds to it, and the refusal must not stop with it.
    let _ = writeln!(s, "add chain inet {table} {}", n.refuse);
    for family in Family::ALL {
        rule(
            &mut s,
            &n.refuse,
            &format!(
                "meta nfproto {} meta l4proto {{ tcp, udp }} add @{} \
                 {{ ct original {} daddr . meta l4proto . ct original proto-dst }}",
                family.nfproto(),
                n.refused(family),
                family.keyword()
            ),
        );
    }
    rule(&mut s, &n.refuse, "goto refuse");

    let mark = table.mark(build_id);
    // Destinations are matched as the build asked for them, before any NAT:
    // Docker's embedded DNS answers 127.0.0.11:53 on another port, and a
    // port that Docker publishes on the host is rewritten to the container's
    // address.
    let _ = writeln!(s, "add chain inet {table} {}", n.chain);
    for view in Family::ALL.map(|family| FamilyPolicy::of(policy, family)) {
        let ip = view.family.keyword();
        if !view.resolvers.is_empty() {
            rule(
                &mut s,
                &n.chain,
                &format!(
                    "meta l4proto {{ tcp, udp }} ct original {ip} daddr {{ {} }} \
                     ct original proto-dst 53 accept",
                    view.resolvers.join(", ")
                ),
            );
        }
        if !view.allowed.is_empty() {
            rule(
                &mut s,
                &n.chain,
                &format!(
                    "meta l4proto {{ tcp, udp }} ct original {ip} daddr . ct original proto-dst \
                     {{ {} }} accept",
                    view.allowed.join(", ")
                ),
            );
        }
    }
    let build = matching(cgroup);
    rule(
        &mut s,
        &n.chain,
        &format!("fib daddr type local meta mark set {mark:#010x} accept"),
    );
    for family in Family::ALL {
        let ip = family.keyword();
        rule(
            &mut s,
            &n.chain,
            &format!(
                "{ip} daddr {{ {} }} goto {}",
                joined(local::fixed(family)),
                n.refuse
            ),
        );
        rule(
            &mut s,
            &n.chain,
            &format!("{ip} daddr @{} goto {}", family.local_set(), n.refuse),
        );
    }
    rule(
        &mut s,
        "each_build_in",
        &format!(
            "meta mark {mark:#010x} {build} accept comment \"{}\"",
            n.chain
        ),
    );
    rule(
        &mut s,
        "each_build_in",
        &format!(
            "meta mark {mark:#010x} goto {} comment \"{}\"",
            n.refuse, n.chain
        ),
    );
    rule(
        &mut s,
        "each_build",
        &format!("{build} jump {} comment \"{}\"", n.chain, n.chain),
    );
    s
}

/// A rule that a build added to a shared chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleHandle {
    pub chain: String,
    pub handle: u64,
}

/// What a build's rules are in the table, read from its JSON listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Installed {
    /// Its rules in the shared chains.
    pub rules: Vec<RuleHandle>,
    /// Its own chains and sets, those present.
    pub chains: Vec<String>,
    pub sets: Vec<String>,
    /// The destinations it was refused.
    pub refused: Vec<Refusal>,
}

/// A destination a build was refused.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Refusal {
    pub addr: IpAddr,
    /// `tcp` or `udp`.
    pub protocol: String,
    pub port: u16,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}",
            SocketAddr::new(self.addr, self.port),
            self.protocol
        )
    }
}

/// Find one build's rules, chains, sets and refusals in its table's listing,
/// `nft -j -a list table inet <table>`.
#[must_use]
pub fn installed(listing: &serde_json::Value, build_id: i32) -> Installed {
    let n = Names::of(build_id);
    let mut found = Installed::default();
    let items = listing
        .get("nftables")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for item in items {
        if let Some(rule) = item.get("rule") {
            let comment = rule.get("comment").and_then(serde_json::Value::as_str);
            let chain = rule.get("chain").and_then(serde_json::Value::as_str);
            let handle = rule.get("handle").and_then(serde_json::Value::as_u64);
            if let (Some(c), Some(chain), Some(handle)) = (comment, chain, handle)
                && c == n.chain
            {
                found.rules.push(RuleHandle {
                    chain: chain.to_string(),
                    handle,
                });
            }
        } else if let Some(chain) = item.get("chain") {
            if let Some(name) = chain.get("name").and_then(serde_json::Value::as_str)
                && (name == n.chain || name == n.refuse)
            {
                found.chains.push(name.to_string());
            }
        } else if let Some(set) = item.get("set")
            && let Some(name) = set.get("name").and_then(serde_json::Value::as_str)
            && (name == n.refused4 || name == n.refused6)
        {
            found.sets.push(name.to_string());
            let elems = set
                .get("elem")
                .and_then(serde_json::Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            found.refused.extend(elems.iter().filter_map(refusal));
        }
    }
    // The build's own chain refers to the other, so it goes first.
    found.chains.sort_by_key(|c| c != &n.chain);
    found.refused.sort();
    found
}

/// One element of a refusal set: `{"concat": [addr, proto, port]}`, bare or
/// wrapped in `{"elem": {"val": ...}}`.
fn refusal(elem: &serde_json::Value) -> Option<Refusal> {
    let concat = elem
        .get("concat")
        .or_else(|| elem.get("elem")?.get("val")?.get("concat"))?
        .as_array()?;
    let [addr, protocol, port] = concat.as_slice() else {
        return None;
    };
    Some(Refusal {
        addr: addr.as_str()?.parse().ok()?,
        protocol: protocol.as_str()?.to_string(),
        port: u16::try_from(port.as_u64()?).ok()?,
    })
}

/// Remove what [`installed`] found, in one transaction.
#[must_use]
pub fn removal_script(table: &Table, found: &Installed) -> String {
    let mut s = String::new();
    for rule in &found.rules {
        let _ = writeln!(
            s,
            "delete rule inet {table} {} handle {}",
            rule.chain, rule.handle
        );
    }
    for chain in &found.chains {
        let _ = writeln!(s, "flush chain inet {table} {chain}");
    }
    for chain in &found.chains {
        let _ = writeln!(s, "delete chain inet {table} {chain}");
    }
    for set in &found.sets {
        let _ = writeln!(s, "delete set inet {table} {set}");
    }
    s
}

/// The `nameserver` lines of a `resolv.conf`, a link-local address's zone
/// dropped.
#[must_use]
pub fn resolvers(resolv_conf: &str) -> Vec<IpAddr> {
    let mut found: Vec<IpAddr> = resolv_conf
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == "nameserver").then_some(())?;
            let addr = words.next()?;
            addr.split('%').next()?.parse().ok()
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// What a `pacman.conf` points pacman at: its `Server` and `CacheServer`
/// URLs, and the files it includes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mirrors {
    pub servers: Vec<Endpoint>,
    pub includes: Vec<String>,
}

/// Read [`Mirrors`] from a `pacman.conf` or a mirrorlist. A `file://` server
/// has no endpoint, and is left out.
#[must_use]
pub fn mirrors(conf: &str) -> Mirrors {
    let mut found = Mirrors::default();
    for line in conf.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Server" | "CacheServer" => {
                if let Some(endpoint) = url::Url::parse(value).ok().as_ref().and_then(|u| {
                    matches!(u.scheme(), "http" | "https")
                        .then(|| Endpoint::of(u))
                        .flatten()
                }) && !found.servers.contains(&endpoint)
                {
                    found.servers.push(endpoint);
                }
            }
            "Include" if !found.includes.iter().any(|i| i == value) => {
                found.includes.push(value.to_string());
            }
            _ => {}
        }
    }
    found
}

/// The server's hosts, refused to builds but for the repository's port.
#[derive(Clone, Debug, Default)]
pub struct ServerHosts {
    /// The repository, which builds are allowed.
    pub repo: Option<Endpoint>,
    /// Where the worker reaches the server's worker protocol.
    pub protocol: Option<Endpoint>,
}

impl ServerHosts {
    fn each(&self) -> impl Iterator<Item = &Endpoint> {
        self.repo.iter().chain(&self.protocol)
    }
}

/// Every address that the server's hosts resolve to now. Blocking: DNS.
///
/// # Errors
/// When one does not resolve.
fn server_addrs(server: &ServerHosts) -> Result<Vec<IpAddr>> {
    let mut addrs = Vec::new();
    for host in server.each() {
        let found = lookup(&host.host, host.port)
            .with_context(|| format!("resolving the server's host {}", host.host))?;
        addrs.extend(found.iter().map(SocketAddr::ip));
    }
    addrs.sort_unstable();
    addrs.dedup();
    Ok(addrs)
}

/// What a build's policy is made from, besides the server's hosts, before any
/// name is resolved.
pub struct Inputs {
    /// Local destinations that every build on this worker may reach
    /// (`WORKER_BUILD_ALLOW`).
    pub allow: Vec<Endpoint>,
    /// The `pacman.conf` the build syncs and installs with.
    pub pacman_conf: PathBuf,
    /// The mirrorlist the server sent for the build, if any.
    pub mirrorlist: Option<String>,
}

/// A policy resolved for one build, with what could not be.
struct Resolved {
    policy: Policy,
    /// The addresses that the server's hosts resolve to: the repository's, and
    /// the worker protocol's.
    server: Vec<IpAddr>,
    /// Allowlist entries whose names did not resolve, already explained.
    skipped: Vec<String>,
}

/// Every address `host` resolves to, deduplicated.
fn lookup(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    let mut addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    addrs.sort_unstable();
    addrs.dedup();
    Ok(addrs)
}

/// `host:port`, an IPv6 address in brackets.
fn shown(endpoint: &Endpoint) -> String {
    if endpoint.host.contains(':') {
        format!("[{}]:{}", endpoint.host, endpoint.port)
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

/// Resolve everything a build's policy names; a server's host that does not
/// resolve adds nothing. Blocking: DNS.
///
/// # Errors
/// When the build's `pacman.conf` cannot be read.
fn resolve(server: &ServerHosts, inputs: &Inputs) -> Result<Resolved> {
    // The build's pacman.conf allows the repository itself.
    let mut server_addrs = Vec::new();
    for host in server.each() {
        if let Ok(addrs) = lookup(&host.host, host.port) {
            server_addrs.extend(addrs.iter().map(SocketAddr::ip));
        }
    }
    server_addrs.sort_unstable();
    server_addrs.dedup();

    // What pacman is pointed at: the build's pacman.conf, what it includes
    // (read here: arch-nspawn gives the chroot the worker's own mirrors), and
    // the mirrorlist the server sent.
    let conf = std::fs::read_to_string(&inputs.pacman_conf)
        .with_context(|| format!("reading {}", inputs.pacman_conf.display()))?;
    let listed = mirrors(&conf);
    let mut servers = listed.servers;
    for include in &listed.includes {
        if let Ok(text) = std::fs::read_to_string(include) {
            servers.extend(mirrors(&text).servers);
        }
    }
    if let Some(list) = &inputs.mirrorlist {
        servers.extend(mirrors(list).servers);
    }
    let mut entries: Vec<Endpoint> = Vec::new();
    for server in servers.into_iter().chain(inputs.allow.iter().cloned()) {
        if !entries.contains(&server) {
            entries.push(server);
        }
    }

    let mut allowed = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries {
        match lookup(&entry.host, entry.port) {
            Ok(addrs) => allowed.extend(addrs),
            Err(e) => skipped.push(format!("{}: {e}", shown(&entry))),
        }
    }
    allowed.sort_unstable();
    allowed.dedup();

    let resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    Ok(Resolved {
        policy: Policy {
            resolvers: resolvers(&resolv),
            allowed,
        },
        server: server_addrs,
        skipped,
    })
}

/// Run `nft` as root, with `script` on its standard input when there is one.
fn nft(args: &[&str], script: Option<&str>) -> Result<String> {
    let mut child = Command::new("sudo")
        .arg("-n")
        .arg("nft")
        .args(args)
        .stdin(if script.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running sudo nft")?;
    if let Some(script) = script
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin
            .write_all(script.as_bytes())
            .context("writing to nft")?;
    }
    let output = child.wait_with_output().context("waiting for nft")?;
    anyhow::ensure!(
        output.status.success(),
        "nft {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Held around every change to this worker's table, which only this process
/// writes: `nft` works a merge into the local sets out from its own copy of
/// them, so of two builds starting at once, the second would delete what the
/// first had already merged, and fail whole.
static LOAD: Mutex<()> = Mutex::new(());

fn load(script: &str) -> Result<()> {
    let _held = LOAD.lock().unwrap_or_else(PoisonError::into_inner);
    nft(&["-f", "-"], Some(script)).map(drop)
}

fn listing(table: &Table) -> Result<serde_json::Value> {
    let raw = nft(&["-j", "-a", "list", "table", "inet", table.name()], None)?;
    serde_json::from_str(&raw).context("parsing nft's listing")
}

/// Remove one build's rules, returning what it was refused.
fn remove(table: &Table, build_id: i32) -> Result<Vec<Refusal>> {
    let found = installed(&listing(table)?, build_id);
    let script = removal_script(table, &found);
    if !script.is_empty() {
        load(&script)?;
    }
    Ok(found.refused)
}

/// The policy in force on this worker.
pub enum BuildNetwork {
    /// `WORKER_BUILD_FIREWALL=0`: builds reach whatever the worker can.
    Unrestricted,
    /// The baseline is in place, and each build gets its own rules.
    Enforced(Firewall),
    /// The policy could not be put in place, for this reason: the worker
    /// takes no builds, and refuses any it holds.
    Unavailable(String),
}

/// The worker's table, its baseline in place on the builds' cgroup.
pub struct Firewall {
    table: Table,
    server: ServerHosts,
}

impl BuildNetwork {
    /// Why this worker can take no build, when the policy could not be put
    /// in place.
    #[must_use]
    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            Self::Unavailable(reason) => Some(reason),
            Self::Unrestricted | Self::Enforced(_) => None,
        }
    }

    /// Make `table` afresh with the baseline on the builds under `cgroups`
    /// and the `server`'s addresses, at startup. Blocking.
    #[must_use]
    pub fn install(table: Table, cgroups: Option<&Hierarchy>, server: ServerHosts) -> Self {
        if disabled() {
            // A `builds/` that outlived the last run is still matched by it.
            let removal = format!("add table inet {table}\ndelete table inet {table}\n");
            if let Err(e) = load(&removal) {
                tracing::debug!("no build network table removed: {e:#}");
            }
            tracing::warn!(
                "{DISABLE_ENV} is off: builds can reach the server's API and worker \
                 protocol, and the worker's local network"
            );
            return Self::Unrestricted;
        }
        match put_in_place(&table, cgroups, &server) {
            Ok(()) => {
                tracing::info!(
                    "Build network policy in place (table inet {table}): builds are \
                     matched by their cgroup"
                );
                Self::Enforced(Firewall { table, server })
            }
            Err(e) => {
                let reason = format!(
                    "the build network policy could not be put in place ({e:#}); setting \
                     {DISABLE_ENV}=0 builds without it, open to the server's API and \
                     worker protocol and to the local network"
                );
                tracing::error!("{reason}; this worker takes no builds");
                Self::Unavailable(reason)
            }
        }
    }

    /// Put one build's rules in place, after its cgroup exists and before it
    /// starts. `None` when there is no policy to apply.
    ///
    /// # Errors
    /// When they cannot be, the build must not run: the baseline alone
    /// refuses the repository pacman needs, and leaves out the build's own
    /// listeners and its record of refusals.
    pub async fn open(
        &self,
        build_id: i32,
        cgroup: Option<&BuildCgroup>,
        inputs: Inputs,
    ) -> Result<Option<BuildRules>> {
        let firewall = match self {
            Self::Unrestricted => return Ok(None),
            Self::Unavailable(reason) => anyhow::bail!("{reason}"),
            Self::Enforced(firewall) => firewall,
        };
        let cgroup = cgroup
            .and_then(BuildCgroup::name)
            .context("the build has no cgroup to match its network rules on")?;
        let table = firewall.table.clone();
        let server = firewall.server.clone();
        tokio::task::spawn_blocking(move || load_build(table, build_id, &cgroup, &server, &inputs))
            .await
            .context("the network rules' task died")?
            .map(Some)
    }
}

/// Resolve one build's policy, read the host's links, and load its rules.
/// Blocking.
///
/// They come back in the guard that removes them, so a build cancelled
/// meanwhile still has them removed when this result is dropped.
fn load_build(
    table: Table,
    build_id: i32,
    cgroup: &CgroupName,
    server: &ServerHosts,
    inputs: &Inputs,
) -> Result<BuildRules> {
    let resolved = resolve(server, inputs)?;
    // What startup read is in the sets already: this only adds to it.
    let links = local::links().unwrap_or_else(|e| {
        tracing::warn!("build {build_id} is refused only the links read at startup: {e:#}");
        Vec::new()
    });
    // A chain left by an earlier attempt under the same id.
    let _ = remove(&table, build_id);
    let script = local_script(&table, &links, &resolved.server)
        + &build_script(&table, build_id, cgroup, &resolved.policy);
    load(&script).context("loading the build's network rules")?;
    Ok(BuildRules {
        table,
        build_id,
        skipped: resolved.skipped,
        open: true,
    })
}

/// Make the table afresh, and check that it holds a process in the builds'
/// cgroup.
///
/// # Errors
/// With why, when builds cannot be matched by their cgroup here, the
/// server's hosts do not resolve, or the host's links cannot be read.
fn put_in_place(table: &Table, cgroups: Option<&Hierarchy>, server: &ServerHosts) -> Result<()> {
    let hierarchy = cgroups.context(
        "builds cannot be matched by their cgroup: there is no cgroup per build (a \
         container needs `privileged`, a native install `Delegate=yes` on its unit)",
    )?;
    let builds = hierarchy.builds_name().with_context(|| {
        format!(
            "builds cannot be matched by their cgroup: nftables cannot name the builds' \
             cgroup {} (a `\"` or a line break in its path)",
            hierarchy.builds_dir().display()
        )
    })?;
    let server = server_addrs(server)?;
    let links = local::links()?;
    load(&(baseline_script(table, &builds) + &local_script(table, &links, &server))).context(
        "nftables refused rules on the builds' cgroup; `socket cgroupv2` needs Linux 5.13 \
         or later",
    )?;
    match probe(hierarchy)
        .context("checking that the rules on the builds' cgroup hold a process there")?
    {
        Probe::Refused => Ok(()),
        Probe::Connected => anyhow::bail!(
            "a process in the builds' cgroup reached a local address the policy refuses: \
             this kernel does not match `socket cgroupv2` from the worker's cgroup \
             namespace. Run the worker's container with `cgroup: host` \
             (`--cgroupns=host`), or on Linux 6.11 or a stable kernel with its fix \
             (design/implemented/build-network.md#where-builds-cannot-be-matched-by-cgroup)"
        ),
    }
}

/// What a process in the builds' cgroup got when it connected to a listener
/// of the worker's on loopback.
enum Probe {
    /// The connection was refused, as the baseline refuses it.
    Refused,
    Connected,
}

/// Check that the baseline holds a process in the builds' cgroup: one placed
/// there connects to a listener of the worker's on loopback, which the
/// baseline refuses with a reset.
///
/// # Errors
/// When the check could not be made, or the connection failed some other way:
/// neither says the rules hold.
fn probe(hierarchy: &Hierarchy) -> Result<Probe> {
    let listener = TcpListener::bind("127.0.0.1:0").context("binding a listener")?;
    let port = listener.local_addr()?.port();
    let cgroup = hierarchy.scratch("network-probe")?;
    let handle = cgroup.procs_handle()?;
    match connect_from(&handle, port)? {
        Connect::Connected => Ok(Probe::Connected),
        Connect::Failed(e) if e.raw_os_error() == Some(libc::ECONNREFUSED) => Ok(Probe::Refused),
        // What `connect` says once `SO_SNDTIMEO` runs out.
        Connect::Failed(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => anyhow::bail!(
            "a connection from the builds' cgroup got no answer within {} s",
            CONNECT_TIMEOUT.tv_sec
        ),
        Connect::Failed(e) => Err(e).context("connecting from the builds' cgroup"),
    }
}

/// How a connection made from another cgroup went.
enum Connect {
    Connected,
    Failed(std::io::Error),
}

/// The exit statuses of [`connect_from`]'s child, besides the `errno` of a
/// failed `connect`, which is always below them.
const CHILD_CONNECTED: i32 = 0;
const CHILD_NOT_JOINED: i32 = 250;
const CHILD_NO_SOCKET: i32 = 251;

/// How long the child waits for its connection. Without it, a connection that
/// the rules dropped rather than refused would wait out TCP's own timeout.
const CONNECT_TIMEOUT: libc::timeval = libc::timeval {
    tv_sec: 5,
    tv_usec: 0,
};

/// Connect to `127.0.0.1:port` from a child placed in the cgroup behind
/// `handle`.
///
/// A forked child, not a program: the socket has to be made inside the
/// cgroup, and its `errno` is the answer.
fn connect_from(handle: &OwnedFd, port: u16) -> Result<Connect> {
    let addr = libc::sockaddr_in {
        sin_family: libc::sa_family_t::try_from(libc::AF_INET)?,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from(Ipv4Addr::LOCALHOST).to_be(),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: the child only runs `connect_in_child`, which makes
    // async-signal-safe calls on values made before the fork, and leaves with
    // `_exit`.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: in the child, as `connect_in_child` requires.
        unsafe { libc::_exit(connect_in_child(handle, &addr)) }
    }
    anyhow::ensure!(pid > 0, "forking: {}", std::io::Error::last_os_error());
    let mut status = 0;
    loop {
        // SAFETY: waits for the child forked above, into a local.
        if unsafe { libc::waitpid(pid, &raw mut status, 0) } == pid {
            break;
        }
        let e = std::io::Error::last_os_error();
        anyhow::ensure!(
            e.kind() == std::io::ErrorKind::Interrupted,
            "waiting for the connecting child: {e}"
        );
    }
    anyhow::ensure!(
        libc::WIFEXITED(status),
        "the connecting child ended with signal {}",
        libc::WTERMSIG(status)
    );
    Ok(match libc::WEXITSTATUS(status) {
        CHILD_CONNECTED => Connect::Connected,
        CHILD_NOT_JOINED => anyhow::bail!("the connecting child could not join its cgroup"),
        CHILD_NO_SOCKET => anyhow::bail!("the connecting child could not open a socket"),
        errno => Connect::Failed(std::io::Error::from_raw_os_error(errno)),
    })
}

/// The child's side of [`connect_from`]: its exit status.
///
/// # Safety
/// Only in a child just forked from this threaded process: every call here is
/// async-signal-safe, and nothing allocates.
unsafe fn connect_in_child(handle: &OwnedFd, addr: &libc::sockaddr_in) -> i32 {
    // SAFETY: the caller is a freshly forked child.
    if unsafe { BuildCgroup::join_current_process(handle) }.is_err() {
        return CHILD_NOT_JOINED;
    }
    // SAFETY: plain system calls on values this function owns or borrows.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return CHILD_NO_SOCKET;
        }
        let timeout = CONNECT_TIMEOUT;
        #[allow(clippy::cast_possible_truncation)]
        let timeout_len = size_of::<libc::timeval>() as libc::socklen_t;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            (&raw const timeout).cast(),
            timeout_len,
        );
        #[allow(clippy::cast_possible_truncation)]
        let addr_len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        if libc::connect(fd, std::ptr::from_ref(addr).cast(), addr_len) == 0 {
            CHILD_CONNECTED
        } else {
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(CHILD_NO_SOCKET)
        }
    }
}

/// One build's rules while they are in place. Removed when dropped.
pub struct BuildRules {
    table: Table,
    build_id: i32,
    skipped: Vec<String>,
    open: bool,
}

impl BuildRules {
    /// Allowlist entries left out because their names did not resolve.
    #[must_use]
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// Remove the rules once the build is over, returning what it was
    /// refused, each with why.
    pub async fn close(mut self) -> Vec<String> {
        self.open = false;
        let build_id = self.build_id;
        let table = self.table.clone();
        let refused = tokio::task::spawn_blocking(move || remove(&table, build_id)).await;
        match refused {
            Ok(Ok(refused)) => refused.iter().map(explain).collect(),
            Ok(Err(e)) => {
                tracing::warn!("could not remove build {build_id}'s network rules: {e:#}");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!("the task removing build {build_id}'s network rules died: {e}");
                Vec::new()
            }
        }
    }
}

impl Drop for BuildRules {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        let build_id = self.build_id;
        let table = self.table.clone();
        let removal = move || {
            if let Err(e) = remove(&table, build_id) {
                tracing::warn!("could not remove build {build_id}'s network rules: {e:#}");
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(removal);
        } else {
            removal();
        }
    }
}

/// A refusal as the build's log says it.
fn explain(refusal: &Refusal) -> String {
    format!(
        "{refusal}: a local address; if the build needs it, the worker's \
         WORKER_BUILD_ALLOW can allow it (host:port)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn cgroup(path: &str) -> CgroupName {
        CgroupName {
            path: path.to_string(),
            level: path.split('/').count(),
        }
    }

    /// A worker's table, named after its fingerprint.
    fn table() -> Table {
        Table::for_worker("5e1f0a2b3c4d5e6f7a8b9c0d1e2f3a4b")
    }

    fn ip(addr: &str) -> IpAddr {
        addr.parse().unwrap()
    }

    fn sock(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    /// The compose deployment: the server and its repository on one address,
    /// Docker's resolver, a LAN mirror.
    fn compose() -> Policy {
        Policy {
            resolvers: vec![ip("127.0.0.11")],
            allowed: vec![sock("172.18.0.2:8081"), sock("192.168.1.5:7878")],
        }
    }

    fn rules(script: &str, chain: &str) -> Vec<String> {
        let prefix = format!("add rule inet {} {chain} ", table());
        script
            .lines()
            .filter_map(|l| l.strip_prefix(&prefix))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_baseline_matches_builds_and_refuses_local_destinations() {
        let script = baseline_script(
            &table(),
            &cgroup("system.slice/aurcache-worker.service/builds"),
        );
        assert!(script.starts_with(
            "add table inet aurcache_worker_5e1f0a2b3c4d5e6f\n\
             delete table inet aurcache_worker_5e1f0a2b3c4d5e6f\n"
        ));
        assert!(script.contains(
            "socket cgroupv2 level 3 \"system.slice/aurcache-worker.service/builds\" jump builds"
        ));
        let builds = script
            .split("chain builds {")
            .nth(1)
            .and_then(|s| s.split("chain each_build {").next())
            .unwrap();
        // Established first, each build's own rules next, the refusals last.
        let order = [
            "ct state established,related accept",
            "jump each_build",
            "fib daddr type local goto refuse",
            "ip daddr {",
            "ip daddr @local4 goto refuse",
            "ip6 daddr {",
            "ip6 daddr @local6 goto refuse",
        ];
        let positions: Vec<_> = order.iter().map(|r| builds.find(r).unwrap()).collect();
        assert!(positions.is_sorted(), "{builds}");
        for range in [
            "192.168.0.0/16",
            "100.64.0.0/10",
            "240.0.0.0/4",
            "fc00::/7",
            "fe80::/10",
        ] {
            assert!(builds.contains(range), "{range} is missing: {builds}");
        }
        assert!(
            script.contains("set local4 {\n\t\ttype ipv4_addr; flags interval; auto-merge;\n\t}")
        );
        assert!(
            script.contains("set local6 {\n\t\ttype ipv6_addr; flags interval; auto-merge;\n\t}")
        );
        assert!(script.contains("reject with tcp reset"));
    }

    #[test]
    fn the_allowlist_names_exact_endpoints_before_the_local_refusal() {
        let script = build_script(&table(), 42, &cgroup("builds/build-42"), &compose());
        let chain = rules(&script, "build-42");
        let allow = chain
            .iter()
            .position(|r| r.ends_with("accept") && r.contains(". ct original proto-dst {"))
            .unwrap();
        let local = chain
            .iter()
            .position(|r| r.starts_with("fib daddr type local"))
            .unwrap();
        assert!(allow < local, "{chain:#?}");
        assert_eq!(
            chain[allow],
            "meta l4proto { tcp, udp } ct original ip daddr . ct original proto-dst \
             { 172.18.0.2 . 8081, 192.168.1.5 . 7878 } accept"
        );
        assert!(
            !chain
                .iter()
                .any(|r| r.contains("8080") || r.contains("8083")),
            "{chain:#?}"
        );
    }

    #[test]
    fn dns_is_matched_as_the_build_asked_for_it() {
        let chain = rules(
            &build_script(&table(), 42, &cgroup("builds/build-42"), &compose()),
            "build-42",
        );
        assert_eq!(
            chain[0],
            "meta l4proto { tcp, udp } ct original ip daddr { 127.0.0.11 } \
             ct original proto-dst 53 accept"
        );
    }

    #[test]
    fn a_builds_own_listeners_are_told_apart_on_the_way_in() {
        let script = build_script(&table(), 42, &cgroup("builds/build-42"), &compose());
        let chain = rules(&script, "build-42");
        let mark = format!("{:#010x}", table().mark(42));
        let local = chain
            .iter()
            .position(|r| *r == format!("fib daddr type local meta mark set {mark} accept"))
            .unwrap();
        let ranges = chain
            .iter()
            .position(|r| r.starts_with("ip daddr {") && r.ends_with("goto build-42-refuse"))
            .unwrap();
        assert!(local < ranges);
        // The host's links and the server's other ports are refused, and
        // recorded, as the build's own.
        for links in [
            "ip daddr @local4 goto build-42-refuse",
            "ip6 daddr @local6 goto build-42-refuse",
        ] {
            let at = chain.iter().position(|r| r == links).unwrap();
            assert!(local < at, "{chain:#?}");
        }
        assert_eq!(
            rules(&script, "each_build_in"),
            [
                format!(
                    "meta mark {mark} socket cgroupv2 level 2 \"builds/build-42\" accept \
                     comment \"build-42\""
                ),
                format!("meta mark {mark} goto build-42-refuse comment \"build-42\""),
            ]
        );
        assert_eq!(
            rules(&script, "each_build"),
            ["socket cgroupv2 level 2 \"builds/build-42\" jump build-42 comment \"build-42\""]
        );
    }

    #[test]
    fn refusals_are_recorded_apart_from_the_refusal() {
        let script = build_script(&table(), 42, &cgroup("builds/build-42"), &compose());
        assert!(script.contains(
            "add set inet aurcache_worker_5e1f0a2b3c4d5e6f build-42-refused4 \
             { type ipv4_addr . inet_proto . inet_service; flags dynamic; size 1024; }"
        ));
        let refuse = rules(&script, "build-42-refuse");
        assert_eq!(refuse.len(), 3);
        assert!(refuse[0].contains("add @build-42-refused4"));
        assert!(refuse[1].contains("add @build-42-refused6"));
        assert_eq!(refuse[2], "goto refuse");
    }

    #[test]
    fn an_empty_family_gets_no_rules() {
        let policy = Policy {
            allowed: vec![sock("10.0.0.2:8081")],
            ..Policy::default()
        };
        let chain = rules(
            &build_script(&table(), 3, &cgroup("builds/build-3"), &policy),
            "build-3",
        );
        let allowing: Vec<&String> = chain.iter().filter(|r| r.contains("ct original")).collect();
        assert_eq!(
            allowing,
            [
                "meta l4proto { tcp, udp } ct original ip daddr . ct original proto-dst \
              { 10.0.0.2 . 8081 } accept"
            ]
        );
    }

    /// Workers sharing a network namespace each make and remove their own
    /// table, and only it.
    #[test]
    fn a_workers_table_is_named_after_it() {
        assert_eq!(table().name(), "aurcache_worker_5e1f0a2b3c4d5e6f");
        assert_eq!(
            Table::for_worker("AB:CD-ef01").name(),
            "aurcache_worker_abcdef01"
        );
        let other = Table::for_worker("0000ffff0000ffff");
        let script = baseline_script(&other, &cgroup("builds"));
        assert!(script.contains("delete table inet aurcache_worker_0000ffff0000ffff\n"));
        assert!(!script.contains(table().name()));
        let build = build_script(&other, 1, &cgroup("builds/build-1"), &compose());
        assert!(
            build
                .lines()
                .all(|l| l.contains(" inet aurcache_worker_0000ffff0000ffff ")),
            "{build}"
        );
    }

    /// The local sets hold the host's links and the server's hosts, wherever
    /// those are. They are only ever added to, so a later reading cannot take
    /// anything out of the sets every build shares; nftables refuses an
    /// element list with nothing in it.
    #[test]
    fn the_local_sets_are_only_added_to() {
        let links = [
            "203.0.113.0/24".parse().unwrap(),
            "10.9.0.0/22".parse().unwrap(),
        ];
        assert_eq!(
            local_script(
                &table(),
                &links,
                &[ip("198.51.100.7"), ip("10.9.2.4"), ip("2001:db8:56:2::10")]
            ),
            "add element inet aurcache_worker_5e1f0a2b3c4d5e6f local4 \
             { 10.9.0.0/22, 10.9.2.4/32, 198.51.100.7/32, 203.0.113.0/24 }\n\
             add element inet aurcache_worker_5e1f0a2b3c4d5e6f local6 { 2001:db8:56:2::10/128 }\n"
        );
        assert_eq!(local_script(&table(), &[], &[]), "");
    }

    #[test]
    fn a_link_is_its_address_under_its_netmask() {
        let prefix = |addr, mask: Option<&str>| local::prefix(ip(addr), mask.map(ip));
        let net = |net: &str| Some(net.parse().unwrap());
        assert_eq!(
            prefix("203.0.113.9", Some("255.255.255.0")),
            net("203.0.113.0/24")
        );
        assert_eq!(
            prefix("2001:db8::9", Some("ffff:ffff::")),
            net("2001:db8::/32")
        );
        assert_eq!(prefix("198.51.100.7", None), net("198.51.100.7/32"));
        assert_eq!(prefix("203.0.113.9", Some("0.0.0.0")), None);
    }

    /// The inputs of a build whose `pacman.conf` names its repository at
    /// `repo`, `host:port`.
    fn inputs_in(dir: &Path, repo: &str) -> Inputs {
        let pacman_conf = dir.join("pacman.conf");
        let conf = format!("[options]\n\n[aurcache]\nServer = http://{repo}/$arch\n");
        std::fs::write(&pacman_conf, conf).unwrap();
        Inputs {
            allow: Vec::new(),
            pacman_conf,
            mirrorlist: None,
        }
    }

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            host: host.to_string(),
            port,
        }
    }

    /// The worker protocol's host is refused but for nothing, beside the
    /// repository's, which the build's `pacman.conf` allows at its port.
    #[test]
    fn the_server_is_its_repositorys_host_and_its_protocols() {
        let dir = tempfile::tempdir().unwrap();
        let server = ServerHosts {
            repo: Some(endpoint("127.0.0.2", 8081)),
            protocol: Some(endpoint("127.0.0.3", 8083)),
        };
        let resolved = resolve(&server, &inputs_in(dir.path(), "127.0.0.2:8081")).unwrap();
        assert_eq!(resolved.server, [ip("127.0.0.2"), ip("127.0.0.3")]);
        assert_eq!(resolved.policy.allowed, [sock("127.0.0.2:8081")]);
        assert_eq!(
            server_addrs(&server).unwrap(),
            [ip("127.0.0.2"), ip("127.0.0.3")]
        );
    }

    /// A server's host that does not resolve at startup leaves the worker
    /// without a policy. For a build it only adds nothing, and a repository
    /// left out says so in the build's log.
    #[test]
    fn a_server_host_that_does_not_resolve_only_matters_at_startup() {
        for server in [
            ServerHosts {
                repo: Some(endpoint("does-not-resolve.invalid", 8081)),
                protocol: None,
            },
            ServerHosts {
                repo: Some(endpoint("127.0.0.2", 8081)),
                protocol: Some(endpoint("does-not-resolve.invalid", 8083)),
            },
        ] {
            assert!(
                server_addrs(&server)
                    .err()
                    .is_some_and(|e| format!("{e:#}").contains("the server's host")),
                "{server:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let server = ServerHosts {
            repo: Some(endpoint("does-not-resolve.invalid", 8081)),
            protocol: Some(endpoint("127.0.0.3", 8083)),
        };
        let inputs = inputs_in(dir.path(), "does-not-resolve.invalid:8081");
        let resolved = resolve(&server, &inputs).unwrap();
        assert_eq!(resolved.server, [ip("127.0.0.3")]);
        assert!(resolved.policy.allowed.is_empty());
        assert!(
            resolved.skipped.len() == 1
                && resolved.skipped[0].starts_with("does-not-resolve.invalid:8081: "),
            "{:?}",
            resolved.skipped
        );
    }

    /// The allowlist comes first, so the repository's own `address:port`
    /// passes before its host's other ports are refused.
    #[test]
    fn the_repository_passes_before_its_host_is_refused() {
        let script = build_script(&table(), 42, &cgroup("builds/build-42"), &compose());
        let chain = rules(&script, "build-42");
        let allowed = chain
            .iter()
            .position(|r| r.contains("daddr . ct original proto-dst"))
            .unwrap();
        let repo = chain
            .iter()
            .position(|r| r == "ip daddr @local4 goto build-42-refuse")
            .unwrap();
        assert!(allowed < repo, "{chain:#?}");
    }

    /// A worker's builds are marked apart from each other.
    #[test]
    fn the_mark_carries_the_build_under_the_tag() {
        for build_id in [1, 42, 0x0123_4567] {
            assert_eq!(table().mark(build_id) & MARK_MASK, MARK_TAG);
            assert_ne!(table().mark(build_id), table().mark(build_id + 1));
        }
        assert_eq!(table().mark(42), MARK_TAG | 42);
    }

    /// A marked packet that no build names is left alone: the input hook
    /// only refuses what a build's own rules say is its.
    #[test]
    fn the_input_hook_claims_only_its_own_builds_marks() {
        let script = baseline_script(&table(), &cgroup("builds"));
        let input = script
            .split("chain input {")
            .nth(1)
            .and_then(|s| s.split("chain each_build_in {").next())
            .unwrap();
        assert!(input.contains("jump each_build_in"), "{input}");
        assert!(!input.contains("refuse"), "{input}");
    }

    /// What `nft -j -a list table` prints, trimmed.
    fn listing_json() -> serde_json::Value {
        serde_json::json!({"nftables": [
            {"metainfo": {"version": "1.1.7"}},
            {"table": {"family": "inet", "name": "aurcache_worker_5e1f0a2b3c4d5e6f", "handle": 1}},
            {"chain": {"family": "inet", "table": "aurcache_worker_5e1f0a2b3c4d5e6f", "name": "each_build", "handle": 3}},
            {"chain": {"family": "inet", "table": "aurcache_worker_5e1f0a2b3c4d5e6f", "name": "build-42-refuse", "handle": 18}},
            {"chain": {"family": "inet", "table": "aurcache_worker_5e1f0a2b3c4d5e6f", "name": "build-42", "handle": 25}},
            {"chain": {"family": "inet", "table": "aurcache_worker_5e1f0a2b3c4d5e6f", "name": "build-420", "handle": 26}},
            {"set": {"family": "inet", "name": "build-42-refused4", "table": "aurcache_worker_5e1f0a2b3c4d5e6f",
                "type": ["ipv4_addr", "inet_proto", "inet_service"], "handle": 16, "flags": ["dynamic"],
                "elem": [{"concat": ["10.0.0.2", "tcp", 8080]}, {"elem": {"val": {"concat": ["10.0.0.2", "tcp", 8083]}}}]}},
            {"set": {"family": "inet", "name": "build-42-refused6", "table": "aurcache_worker_5e1f0a2b3c4d5e6f",
                "type": ["ipv6_addr", "inet_proto", "inet_service"], "handle": 17, "flags": ["dynamic"]}},
            {"rule": {"family": "inet", "table": "aurcache_worker_5e1f0a2b3c4d5e6f", "chain": "each_build", "handle": 35, "comment": "build-42", "expr": []}},
            {"rule": {"family": "inet", "table": "aurcache_worker_5e1f0a2b3c4d5e6f", "chain": "each_build", "handle": 36, "comment": "build-420", "expr": []}},
            {"rule": {"family": "inet", "table": "aurcache_worker_5e1f0a2b3c4d5e6f", "chain": "each_build_in", "handle": 37, "comment": "build-42", "expr": []}},
        ]})
    }

    #[test]
    fn a_builds_rules_are_found_by_name_and_nothing_else() {
        let found = installed(&listing_json(), 42);
        assert_eq!(
            found.rules,
            [
                RuleHandle {
                    chain: "each_build".to_string(),
                    handle: 35
                },
                RuleHandle {
                    chain: "each_build_in".to_string(),
                    handle: 37
                },
            ]
        );
        assert_eq!(found.chains, ["build-42", "build-42-refuse"]);
        assert_eq!(found.sets, ["build-42-refused4", "build-42-refused6"]);
        assert_eq!(
            found
                .refused
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["10.0.0.2:8080/tcp", "10.0.0.2:8083/tcp"]
        );
    }

    #[test]
    fn removal_takes_the_hooks_before_the_chains() {
        let script = removal_script(&table(), &installed(&listing_json(), 42));
        assert_eq!(
            script,
            "delete rule inet aurcache_worker_5e1f0a2b3c4d5e6f each_build handle 35\n\
             delete rule inet aurcache_worker_5e1f0a2b3c4d5e6f each_build_in handle 37\n\
             flush chain inet aurcache_worker_5e1f0a2b3c4d5e6f build-42\n\
             flush chain inet aurcache_worker_5e1f0a2b3c4d5e6f build-42-refuse\n\
             delete chain inet aurcache_worker_5e1f0a2b3c4d5e6f build-42\n\
             delete chain inet aurcache_worker_5e1f0a2b3c4d5e6f build-42-refuse\n\
             delete set inet aurcache_worker_5e1f0a2b3c4d5e6f build-42-refused4\n\
             delete set inet aurcache_worker_5e1f0a2b3c4d5e6f build-42-refused6\n"
        );
        assert_eq!(removal_script(&table(), &installed(&listing_json(), 7)), "");
    }

    #[test]
    fn resolvers_come_from_nameserver_lines() {
        let conf = "# comment\nsearch lan\nnameserver 127.0.0.53\nnameserver fe80::1%eth0\n\
                    nameserver 127.0.0.53\noptions edns0\n";
        assert_eq!(resolvers(conf), [ip("127.0.0.53"), ip("fe80::1")]);
    }

    #[test]
    fn mirrors_are_the_servers_pacman_is_pointed_at() {
        let conf = "[options]\nHoldPkg = pacman\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n\n\
                    [aurcache]\nSigLevel = Optional TrustAll\nServer = http://aurcache:8081/$arch\n\n\
                    [local]\nServer = file:///srv/repo\n";
        let found = mirrors(conf);
        assert_eq!(
            found.servers,
            [Endpoint {
                host: "aurcache".to_string(),
                port: 8081
            }]
        );
        assert_eq!(found.includes, ["/etc/pacman.d/mirrorlist"]);

        let list = "## Germany\nServer = https://mirror.example.de/archlinux/$repo/os/$arch\n\
                    #Server = http://commented.example/\nCacheServer = http://flexo.lan:7878/$repo/os/$arch\n";
        assert_eq!(
            mirrors(list).servers,
            [
                Endpoint {
                    host: "mirror.example.de".to_string(),
                    port: 443
                },
                Endpoint {
                    host: "flexo.lan".to_string(),
                    port: 7878
                },
            ]
        );
    }

    #[test]
    fn a_refusal_says_how_to_allow_it() {
        let refusal = Refusal {
            addr: ip("192.168.1.9"),
            protocol: "tcp".to_string(),
            port: 3632,
        };
        assert_eq!(
            explain(&refusal),
            "192.168.1.9:3632/tcp: a local address; if the build needs it, the \
             worker's WORKER_BUILD_ALLOW can allow it (host:port)"
        );
    }
}
