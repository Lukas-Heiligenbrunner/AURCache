//! What the build network policy holds, against a real kernel and nftables.
//!
//! `#[ignore]` by default: it moves the caller's cgroup ([`Hierarchy::prepare`]),
//! adds tables to the network namespace it runs in, and gives that namespace a
//! link to another. `scripts/test-kernel.sh` runs it as root in a throwaway
//! privileged container, with `nftables`, `python` and `systemd-nspawn`
//! installed.
//!
//! The server (its worker protocol, its API and its repository) and another
//! host on the local network are listeners of this process on loopback
//! addresses, which the policy treats as it treats any local destination. A host on the worker's link, with addresses that no
//! fixed range calls local, is a listener in a network namespace of its own.
//! The build is root inside `systemd-nspawn`, kept in the build's cgroup the
//! way the worker keeps a build's container there (`--keep-unit`): what
//! pacman, and code its dependencies leave for root, run as.

use aurcache_worker::build_firewall::{BuildNetwork, Inputs, Table};
use aurcache_worker::cgroup::{BuildCgroup, BuildLimits, Hierarchy};
use aurcache_worker_core::repo::{Endpoint, RepoSection};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// What the probe inside the container reports: each check's name and what
/// it got (`open`, `refused`, `timeout`, `moved`, or an error).
fn probe_results(stdout: &str) -> BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("probe "))
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Connects to each `name=host:port` it is given, as root inside the
/// container; then has root try to leave the build's cgroup, says whether each
/// move went through, and connects to the server again.
const PROBE: &str = r#"
import os, socket, sys

def reach(host, port):
    try:
        socket.create_connection((host, int(port)), timeout=3).close()
        return "open"
    except ConnectionRefusedError:
        return "refused"
    except socket.timeout:
        return "timeout"
    except OSError as e:
        return "error:" + e.strerror.replace(" ", "_")

def check(name, target):
    host, port = target.rsplit(":", 1)
    print("probe %s=%s" % (name, reach(host.strip("[]"), port)), flush=True)

def move(name, procs):
    try:
        with open(procs, "w") as f:
            f.write(str(os.getpid()))
        got = "moved"
    except OSError as e:
        got = "error:" + e.strerror.replace(" ", "_")
    print("probe %s=%s" % (name, got), flush=True)

own = socket.socket()
own.bind(("127.0.0.1", 0))
own.listen(1)
check("own-listener", "127.0.0.1:%d" % own.getsockname()[1])
for arg in sys.argv[1:]:
    check(*arg.split("=", 1))

# The container sees its own cgroup as the root. Moving to it, or to a cgroup
# hierarchy mounted afresh, must leave this process under the build.
move("escape-to-namespace-root", "/sys/fs/cgroup/cgroup.procs")
os.makedirs("/tmp/cg", exist_ok=True)
if os.system("mount -t cgroup2 none /tmp/cg") == 0:
    move("escape-to-fresh-mount", "/tmp/cg/cgroup.procs")
else:
    print("probe escape-to-fresh-mount=not-mounted", flush=True)
check("server-api-after-escape", sys.argv[1].split("=", 1)[1])
"#;

/// Run `argv` inside `cgroup`, joined between fork and exec as a build is.
fn run_in(cgroup: &BuildCgroup, argv: &[&str]) -> std::process::Output {
    let handle = cgroup.procs_handle().expect("opening cgroup.procs");
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..]);
    // SAFETY: one write to a descriptor opened before the fork.
    unsafe {
        cmd.pre_exec(move || BuildCgroup::join_current_process(&handle));
    }
    cmd.output().expect("running the child")
}

/// A listener of this process, open for as long as this is kept.
struct Listener {
    _socket: TcpListener,
    addr: SocketAddr,
}

impl Listener {
    fn port(&self) -> u16 {
        self.addr.port()
    }

    /// `name=host:port`, as the probe takes it.
    fn target(&self, name: &str) -> String {
        format!("{name}={}", self.addr)
    }
}

fn listen(addr: impl ToSocketAddrs) -> Listener {
    let socket = TcpListener::bind(addr).expect("binding a listener");
    let addr = socket.local_addr().unwrap();
    Listener {
        _socket: socket,
        addr,
    }
}

/// What one check of the probe should get.
struct Want {
    check: &'static str,
    got: &'static str,
}

fn wanted(checks: &[Want]) -> BTreeMap<String, String> {
    checks
        .iter()
        .map(|want| (want.check.to_string(), want.got.to_string()))
        .collect()
}

/// The network namespace of [`Lan`]'s host, and the link to it.
const LAN_NETNS: &str = "aurcache-lan";
const LAN_LINK: &str = "aurcache-lan0";

/// A host on the worker's link, in a network namespace of its own.
///
/// The link's prefixes are a public IPv4 one and a global IPv6 one, which no
/// fixed range calls local. Behind the host, routed through it, are an IPv4
/// and an IPv6 address that stand for the internet. Removed when dropped.
struct Lan;

/// Listeners on [`Lan`]'s host: beyond the link, through the host as a
/// router.
struct LanListeners {
    beyond_v4: Listener,
    beyond_v6: Listener,
}

impl Lan {
    fn up() -> Self {
        let lan = Self;
        for command in [
            "ip netns add aurcache-lan",
            "ip link add aurcache-lan0 type veth peer name lan1 netns aurcache-lan",
            "ip addr add 203.0.113.1/24 dev aurcache-lan0",
            "ip -6 addr add 2001:db8:113::1/64 dev aurcache-lan0 nodad",
            "ip link set aurcache-lan0 up",
            "ip -n aurcache-lan link set lo up",
            "ip -n aurcache-lan addr add 203.0.113.5/24 dev lan1",
            "ip -n aurcache-lan -6 addr add 2001:db8:113::5/64 dev lan1 nodad",
            "ip -n aurcache-lan addr add 198.51.100.7/32 dev lo",
            "ip -n aurcache-lan -6 addr add 2001:db8:100::7/128 dev lo",
            "ip -n aurcache-lan link set lan1 up",
            "ip route add 198.51.100.0/24 via 203.0.113.5",
            "ip -6 route add 2001:db8:100::/64 via 2001:db8:113::5",
        ] {
            let argv: Vec<&str> = command.split_whitespace().collect();
            let status = Command::new(argv[0])
                .args(&argv[1..])
                .status()
                .expect("running ip");
            assert!(status.success(), "{command} failed: {status}");
        }
        lan
    }

    fn listen(&self) -> LanListeners {
        let [beyond_v4, beyond_v6] = self.listen_on(["198.51.100.7", "2001:db8:100::7"]);
        LanListeners {
            beyond_v4,
            beyond_v6,
        }
    }

    /// A listener on the host at each of `addrs`, on a port of its own.
    fn listen_on<const N: usize>(&self, addrs: [&'static str; N]) -> [Listener; N] {
        let netns = Path::new("/run/netns").join(LAN_NETNS);
        // On a thread of its own: `setns` moves the calling thread only, and a
        // socket stays in the namespace it was made in.
        std::thread::spawn(move || {
            let ns = std::fs::File::open(&netns).expect("opening the host's namespace");
            // SAFETY: moves this thread, which ends here, with a descriptor it
            // holds open.
            let moved = unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) };
            assert_eq!(moved, 0, "setns: {}", std::io::Error::last_os_error());
            addrs.map(|addr| listen(SocketAddr::new(addr.parse::<IpAddr>().unwrap(), 0)))
        })
        .join()
        .expect("listening on the LAN host")
    }
}

impl Drop for Lan {
    fn drop(&mut self) {
        // The peer, and the routes through the link, go with it.
        let _ = Command::new("ip").args(["link", "del", LAN_LINK]).status();
        let _ = Command::new("ip")
            .args(["netns", "del", LAN_NETNS])
            .status();
    }
}

/// A `pacman.conf` whose repository is at one address, or at one name.
struct PacmanConf {
    _dir: tempfile::TempDir,
    path: PathBuf,
    section: RepoSection,
}

/// A `pacman.conf` whose repository is at `127.0.0.1:repo`.
fn pacman_conf(repo: u16) -> PacmanConf {
    pacman_conf_at(SocketAddr::from(([127, 0, 0, 1], repo)))
}

fn pacman_conf_at(repo: SocketAddr) -> PacmanConf {
    pacman_conf_named(&format!("http://{repo}"))
}

/// A `pacman.conf` whose repository is at `server`, a URL.
fn pacman_conf_named(server: &str) -> PacmanConf {
    let dir = tempfile::tempdir().unwrap();
    let section = format!("[aurcache]\nSigLevel = Never\nServer = {server}/$arch\n");
    let path = dir.path().join("pacman.conf");
    std::fs::write(&path, format!("[options]\n\n{section}")).unwrap();
    PacmanConf {
        _dir: dir,
        path,
        section: RepoSection::parse(section).expect("a repository section"),
    }
}

impl PacmanConf {
    fn inputs(&self) -> Inputs {
        Inputs {
            repo: Some(self.section.clone()),
            server: None,
            pacman_conf: self.path.clone(),
            mirrorlist: None,
        }
    }
}

/// What the probe got as root in a container kept in `cgroup`, connecting
/// to each `name=host:port` of `targets`.
fn probe_in(cgroup: &BuildCgroup, targets: &[String]) -> Probed {
    let mut argv = vec![
        "systemd-nspawn",
        "--quiet",
        "--directory=/",
        "--volatile=yes",
        "--keep-unit",
        "--register=no",
        "--as-pid2",
        "--console=pipe",
        "--setenv=PATH=/usr/bin",
        "python3",
        "-c",
        PROBE,
    ];
    argv.extend(targets.iter().map(String::as_str));
    let output = run_in(cgroup, &argv);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "systemd-nspawn failed: {}\n{stdout}\n{stderr}",
        output.status
    );
    Probed {
        results: probe_results(&stdout),
        output: format!("\n{stdout}\n{stderr}"),
    }
}

/// What [`probe_in`] got, and its output to show when that is wrong.
struct Probed {
    results: BTreeMap<String, String>,
    output: String,
}

/// The policy put in place on `hierarchy`'s builds, in `table`.
fn enforced(table: Table, hierarchy: &Hierarchy) -> BuildNetwork {
    let network = BuildNetwork::install(table, Some(hierarchy));
    assert!(
        matches!(network, BuildNetwork::Enforced(_)),
        "`socket cgroupv2` should match builds here (see the error above)"
    );
    network
}

/// Whether `nft list <what>` finds it.
fn nft_lists(what: &[&str]) -> bool {
    Command::new("nft")
        .arg("list")
        .args(what)
        .output()
        .expect("running nft")
        .status
        .success()
}

#[tokio::test]
#[ignore = "restructures the caller's cgroup, adds nftables tables and a network namespace; run in a privileged container"]
async fn root_in_a_builds_container_is_held_by_its_cgroup() {
    // The server's worker protocol, API and repository, and a service
    // elsewhere on this host that nothing allows.
    let protocol = listen("127.0.0.1:0");
    let api = listen("127.0.0.1:0");
    let repo = listen("127.0.0.1:0");
    let nas = listen("127.0.0.2:0");
    let hierarchy = Hierarchy::prepare().expect("preparing the cgroup hierarchy");
    let network = enforced(Table::for_worker("c9a0"), &hierarchy);

    // A router on a link of the worker's, and addresses beyond it.
    let lan = Lan::up();
    let on_lan = lan.listen();

    let conf = pacman_conf(repo.port());
    let cgroup = hierarchy
        .for_build(4242, &BuildLimits::default())
        .expect("creating the build cgroup");
    let rules = network
        .open(4242, Some(&cgroup), conf.inputs())
        .await
        .expect("loading the build's rules")
        .expect("rules, with the policy enforced");

    let targets = [
        api.target("server-api"),
        protocol.target("worker-protocol"),
        repo.target("repository"),
        nas.target("local-network"),
        on_lan.beyond_v4.target("beyond-gateway-v4"),
        on_lan.beyond_v6.target("beyond-gateway-v6"),
    ];
    let probed = probe_in(&cgroup, &targets);
    let want = wanted(&[
        Want {
            check: "own-listener",
            got: "open",
        },
        Want {
            check: "server-api",
            got: "refused",
        },
        Want {
            check: "worker-protocol",
            got: "refused",
        },
        Want {
            check: "repository",
            got: "open",
        },
        Want {
            check: "local-network",
            got: "refused",
        },
        Want {
            check: "beyond-gateway-v4",
            got: "open",
        },
        Want {
            check: "beyond-gateway-v6",
            got: "open",
        },
        // Both moves go through, so the check after them is of a process that
        // got as far out as it could.
        Want {
            check: "escape-to-namespace-root",
            got: "moved",
        },
        Want {
            check: "escape-to-fresh-mount",
            got: "moved",
        },
        Want {
            check: "server-api-after-escape",
            got: "refused",
        },
    ]);
    assert_eq!(probed.results, want, "{}", probed.output);

    let refused = rules.close().await;
    let reported = |listener: &Listener| {
        let needle = format!("{}/tcp", listener.addr);
        refused.iter().any(|r| r.contains(&needle))
    };
    for listener in [&protocol, &api, &nas] {
        assert!(
            reported(listener),
            "the refusal of {} is not reported: {refused:#?}",
            listener.addr
        );
    }
    for listener in [&repo, &on_lan.beyond_v4, &on_lan.beyond_v6] {
        assert!(!reported(listener), "{refused:#?}");
    }

    // The worker itself is not a build: it still reaches all of them.
    for listener in [&protocol, &api, &repo, &nas] {
        TcpStream::connect(listener.addr)
            .unwrap_or_else(|e| panic!("the worker cannot reach {}: {e}", listener.addr));
    }
}

/// A build whose future is dropped while its rules are being loaded still
/// has them removed: the loading runs to the end, and what it hands back
/// removes them when nothing takes it.
#[tokio::test]
#[ignore = "restructures the caller's cgroup and adds nftables tables; run in a privileged container"]
async fn the_rules_of_a_build_cancelled_while_they_load_are_removed() {
    let hierarchy = Hierarchy::prepare().expect("preparing the cgroup hierarchy");
    let table = Table::for_worker("ca9ce1");
    let network = enforced(table.clone(), &hierarchy);
    let repo = listen("127.0.0.1:0");
    let conf = pacman_conf(repo.port());
    let cgroup = hierarchy
        .for_build(4343, &BuildLimits::default())
        .expect("creating the build cgroup");

    // What nftables itself saw: that the cancelled build's rules did go in,
    // and not only that none are left.
    let mut monitor = Command::new("nft")
        .args(["monitor", "chains"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("running nft monitor");
    // Until it listens: a change made before is not reported.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let opened = tokio::time::timeout(
        Duration::from_millis(1),
        network.open(4343, Some(&cgroup), conf.inputs()),
    )
    .await;
    assert!(
        opened.is_err(),
        "the rules were in place within a millisecond, so nothing was cancelled"
    );

    // Loading them, then removing them, takes a few runs of `nft`.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = monitor.kill();
    let seen = monitor.wait_with_output().expect("reading nft monitor");
    let seen = String::from_utf8_lossy(&seen.stdout);
    let chain = format!("chain inet {} build-4343", table.name());
    assert!(
        seen.lines()
            .any(|l| l.starts_with("add ") && l.contains(&chain)),
        "the cancelled build's rules were never loaded:\n{seen}"
    );
    assert!(
        seen.lines()
            .any(|l| l.starts_with("delete ") && l.contains(&chain)),
        "the cancelled build's rules were not removed:\n{seen}"
    );
    assert!(nft_lists(&["table", "inet", table.name()]));
    for chain in ["build-4343", "build-4343-refuse"] {
        assert!(
            !nft_lists(&["chain", "inet", table.name(), chain]),
            "{chain} was left behind by a cancelled build"
        );
    }
}

/// A repository beyond a router, on an address no range calls local, as a
/// server on another VLAN of a routed IPv6 network is: its port is reached,
/// and the other ports of its host, where the server's API and worker
/// protocol would be, are refused. Another address there stays reachable.
#[tokio::test]
#[ignore = "restructures the caller's cgroup, adds nftables tables and a network namespace; run in a privileged container"]
async fn a_routed_repositorys_host_is_reached_only_at_the_repository() {
    let hierarchy = Hierarchy::prepare().expect("preparing the cgroup hierarchy");
    let network = enforced(Table::for_worker("4e90"), &hierarchy);
    let lan = Lan::up();
    let [api, repo, internet] = lan.listen_on(["198.51.100.7", "198.51.100.7", "2001:db8:100::7"]);
    let conf = pacman_conf_at(repo.addr);
    let cgroup = hierarchy
        .for_build(4444, &BuildLimits::default())
        .expect("creating the build cgroup");
    let rules = network
        .open(4444, Some(&cgroup), conf.inputs())
        .await
        .expect("loading the build's rules")
        .expect("rules, with the policy enforced");

    let probed = probe_in(
        &cgroup,
        &[
            api.target("server-api"),
            repo.target("repository"),
            internet.target("internet"),
        ],
    );
    let got: BTreeMap<String, String> = probed
        .results
        .into_iter()
        .filter(|(check, _)| ["server-api", "repository", "internet"].contains(&check.as_str()))
        .collect();
    let want = wanted(&[
        Want {
            check: "server-api",
            got: "refused",
        },
        Want {
            check: "repository",
            got: "open",
        },
        Want {
            check: "internet",
            got: "open",
        },
    ]);
    assert_eq!(got, want, "{}", probed.output);

    let refused = rules.close().await;
    let needle = format!("{}/tcp", api.addr);
    assert!(
        refused.iter().any(|r| r.contains(&needle)),
        "the refusal of {} is not reported: {refused:#?}",
        api.addr
    );
    drop(lan);
}

/// The worker protocol on a routed address, away from the repository: its
/// host is refused on every port, as the repository's is, though nothing
/// there is allowed. Another routed address stays reachable.
#[tokio::test]
#[ignore = "restructures the caller's cgroup, adds nftables tables and a network namespace; run in a privileged container"]
async fn a_routed_servers_host_is_refused_though_the_repository_is_elsewhere() {
    let hierarchy = Hierarchy::prepare().expect("preparing the cgroup hierarchy");
    let network = enforced(Table::for_worker("5e7e"), &hierarchy);
    let lan = Lan::up();
    let [protocol, api, internet] =
        lan.listen_on(["198.51.100.7", "198.51.100.7", "2001:db8:100::7"]);
    let repo = listen("127.0.0.1:0");
    let conf = pacman_conf(repo.port());
    let cgroup = hierarchy
        .for_build(4545, &BuildLimits::default())
        .expect("creating the build cgroup");
    let rules = network
        .open(
            4545,
            Some(&cgroup),
            Inputs {
                server: Some(Endpoint {
                    host: protocol.addr.ip().to_string(),
                    port: protocol.port(),
                }),
                ..conf.inputs()
            },
        )
        .await
        .expect("loading the build's rules")
        .expect("rules, with the policy enforced");

    let probed = probe_in(
        &cgroup,
        &[
            protocol.target("worker-protocol"),
            api.target("server-api"),
            repo.target("repository"),
            internet.target("internet"),
        ],
    );
    let checks = ["worker-protocol", "server-api", "repository", "internet"];
    let got: BTreeMap<String, String> = probed
        .results
        .into_iter()
        .filter(|(check, _)| checks.contains(&check.as_str()))
        .collect();
    let want = wanted(&[
        Want {
            check: "worker-protocol",
            got: "refused",
        },
        Want {
            check: "server-api",
            got: "refused",
        },
        Want {
            check: "repository",
            got: "open",
        },
        Want {
            check: "internet",
            got: "open",
        },
    ]);
    assert_eq!(got, want, "{}", probed.output);
    let _ = rules.close().await;
    drop(lan);
}

/// A build whose repository's name does not resolve is refused, and the
/// server's sets, which every build of the worker shares, keep what a running
/// build filled them with: its refusal of the server's other ports holds.
#[tokio::test]
#[ignore = "restructures the caller's cgroup, adds nftables tables and a network namespace; run in a privileged container"]
async fn a_repository_that_does_not_resolve_refuses_the_build_and_leaves_the_others_held() {
    let hierarchy = Hierarchy::prepare().expect("preparing the cgroup hierarchy");
    let network = enforced(Table::for_worker("dn5"), &hierarchy);
    let lan = Lan::up();
    let [api, repo] = lan.listen_on(["198.51.100.7", "198.51.100.7"]);
    let conf = pacman_conf_at(repo.addr);
    let running = hierarchy
        .for_build(4646, &BuildLimits::default())
        .expect("creating the build cgroup");
    let rules = network
        .open(4646, Some(&running), conf.inputs())
        .await
        .expect("loading the running build's rules")
        .expect("rules, with the policy enforced");

    let unresolved = pacman_conf_named("http://does-not-resolve.invalid:8081");
    let next = hierarchy
        .for_build(4647, &BuildLimits::default())
        .expect("creating the build cgroup");
    let opened = network.open(4647, Some(&next), unresolved.inputs()).await;

    let probed = probe_in(&running, &[api.target("server-api")]);
    assert_eq!(
        probed.results.get("server-api").map(String::as_str),
        Some("refused"),
        "the running build reaches the server's API once the next one is opened: {}",
        probed.output
    );
    assert!(
        opened.is_err(),
        "a build whose repository does not resolve got rules"
    );
    let _ = rules.close().await;
    drop(lan);
}

/// Workers sharing a network namespace each keep a table of their own: one
/// starting leaves the other's in place.
#[tokio::test]
#[ignore = "restructures the caller's cgroup and adds nftables tables; run in a privileged container"]
async fn a_worker_starting_leaves_anothers_table_alone() {
    let hierarchy = Hierarchy::prepare().expect("preparing the cgroup hierarchy");
    let first = Table::for_worker("f125");
    let second = Table::for_worker("5ec0d");
    let install = |table: &Table| enforced(table.clone(), &hierarchy);
    let _first = install(&first);
    let _second = install(&second);
    assert!(nft_lists(&["table", "inet", first.name()]));
    assert!(nft_lists(&["table", "inet", second.name()]));
    // A restart makes its own table afresh, and only its own.
    let _first = install(&first);
    assert!(nft_lists(&["table", "inet", second.name()]));
}
