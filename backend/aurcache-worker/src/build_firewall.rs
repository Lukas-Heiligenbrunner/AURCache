//! Keep package builds off the worker-protocol port.
//!
//! Builds share the worker host's network namespace (systemd-nspawn runs
//! without `--network-*` flags), so a crafted PKGBUILD can reach the server's
//! worker port: it learns the address from its own `pacman.conf`, fetches the
//! public CA, and submits a rogue registration under a spoofed name. That
//! lands `pending`, one mistaken approval from a valid mTLS certificate.
//!
//! The repo, the AUR and the internet must stay reachable, so the block is by
//! (`uid`, `port`) rather than by host: every build runs as the unprivileged
//! build user (`WORKER_BUILD_USER`, `builder` by default), while the worker
//! itself runs as `aurcache`. One `OUTPUT` rule per address family rejects the
//! build user's TCP traffic to the port the worker actually dials, and nothing
//! else.
//!
//! That port is not always the worker's own: a server reached as
//! `https://aurcache.example.com` (TLS passthrough on 443) shares its port with
//! the whole web, and possibly with the repository. Then the rule is narrowed
//! to the addresses the server's host resolves to, and when the repository is
//! served on one of those same addresses and ports no rule is installed at
//! all, since blocking the worker protocol would block the repository with it.
//!
//! Set `WORKER_BUILD_FIREWALL=0` to skip this (exotic network setups); the
//! worker logs a warning either way when the rule cannot be installed, and
//! never refuses to start over it.

use anyhow::{Context, Result};
use std::net::{IpAddr, ToSocketAddrs};
use std::process::Command;

/// Environment switch that disables the rule; see the module docs.
const DISABLE_ENV: &str = "WORKER_BUILD_FIREWALL";

/// Ports builds need towards any host: how sources, the AUR and mirrors are
/// fetched. A worker protocol reached on one of them can only be blocked
/// host by host.
const WEB_PORTS: [u16; 2] = [80, 443];

/// Whether the operator disabled the rule.
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

/// Where a URL's traffic goes: its host and the port actually dialled.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    host: String,
    port: u16,
}

/// The endpoint a URL is dialled on.
///
/// The port is the explicit one, else the scheme's default -- what the HTTP
/// client connects to, so what the rule has to name. An IPv6 literal is
/// returned without its brackets, ready for resolution.
fn endpoint(url: &str) -> Option<Endpoint> {
    let url = url::Url::parse(url).ok()?;
    let host = match url.host()? {
        url::Host::Domain(domain) => domain.to_string(),
        url::Host::Ipv4(addr) => addr.to_string(),
        url::Host::Ipv6(addr) => addr.to_string(),
    };
    Some(Endpoint {
        host,
        port: url.port_or_known_default()?,
    })
}

/// Every address an endpoint's host resolves to, deduplicated.
///
/// Resolved once, at startup: an address that changes afterwards is not
/// followed until the worker restarts.
fn resolve(endpoint: &Endpoint) -> Vec<IpAddr> {
    let mut addrs: Vec<IpAddr> = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map(|found| found.map(|a| a.ip()).collect())
        .unwrap_or_default();
    addrs.sort_unstable();
    addrs.dedup();
    addrs
}

/// An endpoint together with the addresses its host resolved to.
struct Resolved<'a> {
    endpoint: &'a Endpoint,
    addrs: &'a [IpAddr],
}

/// Which destinations the rule covers, besides the worker port.
#[derive(Debug, PartialEq, Eq)]
enum Scope {
    /// The port alone: nothing else a build needs is served on it.
    AnyHost,
    /// Only these addresses, because the port is shared with what builds
    /// must still reach.
    Addresses(Vec<IpAddr>),
}

/// Decide the rule's scope, refusing one that would block the repository.
fn scope(worker: &Resolved, repo: Option<&Resolved>) -> Result<Scope> {
    let port = worker.endpoint.port;
    let repo_on_port = repo.filter(|r| r.endpoint.port == port);
    if !WEB_PORTS.contains(&port) && repo_on_port.is_none() {
        return Ok(Scope::AnyHost);
    }
    anyhow::ensure!(
        !worker.addrs.is_empty(),
        "the worker port {port} is shared with other traffic, and {} did not \
         resolve to narrow the rule to",
        worker.endpoint.host
    );
    if let Some(repo) = repo_on_port {
        anyhow::ensure!(
            !repo.addrs.iter().any(|a| worker.addrs.contains(a)),
            "the package repository ({}:{port}) is served on the same address and \
             port as the worker protocol, so blocking one would block both; give \
             the worker protocol a port of its own",
            repo.endpoint.host
        );
    }
    Ok(Scope::Addresses(worker.addrs.to_vec()))
}

/// Refuse a rule that would match this process itself.
///
/// A `WORKER_BUILD_USER` naming the worker's own account (or root, via
/// `sudo -u` inheritance accidents) would firewall the worker off its own
/// server: enrollment, claims and heartbeats would all fail closed with
/// nothing pointing at this rule. The build user exists precisely to be
/// someone else; overlapping it is a misconfiguration, and loud is better
/// than a mysteriously catatonic worker.
fn check_not_self(build_uid: u32, own_uid: u32, build_user: &str) -> Result<()> {
    anyhow::ensure!(
        build_uid != own_uid,
        "build user {build_user:?} resolves to this process's own uid ({own_uid}); \
         refusing a rule that would block the worker itself"
    );
    Ok(())
}

/// Numeric uid of a local user, resolved at runtime.
///
/// The build user's uid is assigned dynamically (systemd-sysusers `-`), so it
/// must never be hardcoded: this host gave `builder` 967, another may not.
fn uid_of_user(username: &str) -> Result<u32> {
    aurcache_chroot::Owner::of_user(username)
        .map(|owner| owner.uid)
        .with_context(|| format!("no user named {username:?}"))
}

/// The `OUTPUT` rule that confines the build user, as argv after the tool.
///
/// Idempotent installation is check-then-add: `-C` reports presence, `-I`
/// inserts at the head only when absent.
fn rule_args(uid: u32, port: u16, destination: Option<IpAddr>) -> Vec<String> {
    let mut args = vec![
        "OUTPUT".to_string(),
        "-m".to_string(),
        "owner".to_string(),
        "--uid-owner".to_string(),
        uid.to_string(),
    ];
    if let Some(destination) = destination {
        args.extend(["-d".to_string(), destination.to_string()]);
    }
    args.extend(["-p", "tcp", "--dport", &port.to_string(), "-j", "REJECT"].map(str::to_string));
    args
}

fn run_sudo(
    tool: &str,
    args: &[String],
    path_override: Option<&str>,
) -> Result<std::process::Output> {
    let mut command = Command::new("sudo");
    // Tests point this at a stub toolbox; production always passes `None`,
    // which inherits this process's `PATH` untouched.
    if let Some(path) = path_override {
        command.env("PATH", path);
    }
    command
        .arg(tool)
        .args(args)
        .output()
        .with_context(|| format!("running sudo {tool}"))
}

/// Ensure the rule exists for one address family (`iptables` or `ip6tables`).
fn ensure_one(tool: &str, uid: u32, port: u16, destination: Option<IpAddr>) -> Result<()> {
    ensure_one_in(tool, uid, port, destination, None)
}

fn ensure_one_in(
    tool: &str,
    uid: u32,
    port: u16,
    destination: Option<IpAddr>,
    path_override: Option<&str>,
) -> Result<()> {
    let args = rule_args(uid, port, destination);
    let mut check = vec!["-C".to_string()];
    check.extend(args.clone());
    if run_sudo(tool, &check, path_override)?.status.success() {
        return Ok(());
    }
    let mut add = vec!["-I".to_string()];
    add.extend(args);
    let output = run_sudo(tool, &add, path_override)?;
    anyhow::ensure!(
        output.status.success(),
        "{} refused the build-user worker-port rule: {}",
        tool,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Block the build user's TCP traffic to the worker-protocol port.
///
/// Resolves the build user's uid, the port `server_url` is dialled on and the
/// repository `repo_section` points builds at, then ensures the rule via
/// `iptables` and `ip6tables`. Called after enrollment, since the repository
/// comes from the server, and before any build. A no-op when disabled; an
/// `Err` names what failed so the caller can warn and continue.
///
/// A rule on the port alone is best-effort per family (a v6-less host fails
/// only its own leg); one narrowed to addresses must land for every address.
///
/// The rule is intentionally never removed -- not on shutdown, not on package
/// removal. It names only the build user's uid and one destination port, so a
/// stale rule affects nobody else, while remove-on-stop could never be
/// reliable anyway: a killed worker would leave it behind, and two workers
/// sharing a build user would race to delete each other's. Restarts are safe
/// because installation is check-then-add.
pub fn ensure(server_url: &str, repo_section: &str, build_user: &str) -> Result<()> {
    if disabled() {
        tracing::info!("{DISABLE_ENV} disables the build-user worker-port rule; skipping");
        return Ok(());
    }
    let uid = uid_of_user(build_user)?;
    check_not_self(uid, aurcache_chroot::Owner::current().uid, build_user)?;
    let worker = endpoint(server_url)
        .with_context(|| format!("no host and port to firewall in {server_url:?}"))?;
    let worker_addrs = resolve(&worker);
    // The arch only fills in `$arch`; the host and port are the same for all.
    let repo = aurcache_worker_core::repo::repo_db_url(repo_section, "x86_64")
        .as_deref()
        .and_then(endpoint);
    let repo_addrs = repo.as_ref().map(resolve).unwrap_or_default();
    let scope = scope(
        &Resolved {
            endpoint: &worker,
            addrs: &worker_addrs,
        },
        repo.as_ref()
            .map(|endpoint| Resolved {
                endpoint,
                addrs: &repo_addrs,
            })
            .as_ref(),
    )?;
    let port = worker.port;
    match scope {
        Scope::AnyHost => {
            ensure_one("iptables", uid, port, None)?;
            if let Err(e) = ensure_one("ip6tables", uid, port, None) {
                tracing::debug!("ip6tables leg of the worker-port rule not installed: {e:#}");
            }
        }
        Scope::Addresses(addrs) => {
            tracing::info!(
                "Port {port} is shared with other traffic; firewalling the build user \
                 off it on {} only",
                addrs
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            for addr in addrs {
                let tool = if addr.is_ipv4() {
                    "iptables"
                } else {
                    "ip6tables"
                };
                ensure_one(tool, uid, port, Some(addr))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn at(host: &str, port: u16) -> Endpoint {
        Endpoint {
            host: host.to_string(),
            port,
        }
    }

    fn ip(addr: &str) -> IpAddr {
        addr.parse().unwrap()
    }

    #[test]
    fn the_port_is_the_one_the_client_dials() {
        assert_eq!(
            endpoint("https://aurcache:8083"),
            Some(at("aurcache", 8083))
        );
        assert_eq!(
            endpoint("https://build.example.com:9090/api"),
            Some(at("build.example.com", 9090))
        );
        assert_eq!(
            endpoint("https://example.com"),
            Some(at("example.com", 443))
        );
        assert_eq!(
            endpoint("http://example.com/api"),
            Some(at("example.com", 80))
        );
        assert_eq!(endpoint("https://[::1]:8083"), Some(at("::1", 8083)));
        assert_eq!(endpoint("https://[::1]/api"), Some(at("::1", 443)));
        assert_eq!(endpoint("not a url"), None);
    }

    #[test]
    fn a_port_of_its_own_is_blocked_towards_every_host() {
        let worker = at("aurcache", 8083);
        let repo = at("aurcache", 8081);
        let addrs = [ip("10.0.0.2")];
        let scope = scope(
            &Resolved {
                endpoint: &worker,
                addrs: &addrs,
            },
            Some(&Resolved {
                endpoint: &repo,
                addrs: &addrs,
            }),
        )
        .unwrap();
        assert_eq!(scope, Scope::AnyHost);
    }

    #[test]
    fn a_web_port_is_blocked_towards_the_server_only() {
        let worker = at("aurcache.example.com", 443);
        let addrs = [ip("192.0.2.1"), ip("2001:db8::1")];
        let scope = scope(
            &Resolved {
                endpoint: &worker,
                addrs: &addrs,
            },
            None,
        )
        .unwrap();
        assert_eq!(scope, Scope::Addresses(addrs.to_vec()));
    }

    #[test]
    fn a_repository_on_another_address_stays_reachable() {
        let worker = at("workers.example.com", 443);
        let repo = at("repo.example.com", 443);
        let scope = scope(
            &Resolved {
                endpoint: &worker,
                addrs: &[ip("192.0.2.1")],
            },
            Some(&Resolved {
                endpoint: &repo,
                addrs: &[ip("192.0.2.2")],
            }),
        )
        .unwrap();
        assert_eq!(scope, Scope::Addresses(vec![ip("192.0.2.1")]));
    }

    #[test]
    fn the_repository_is_never_blocked_with_the_worker_port() {
        let worker = at("aurcache.example.com", 443);
        let repo = at("aurcache.example.com", 443);
        let addrs = [ip("192.0.2.1")];
        let err = scope(
            &Resolved {
                endpoint: &worker,
                addrs: &addrs,
            },
            Some(&Resolved {
                endpoint: &repo,
                addrs: &addrs,
            }),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("repository"), "{err:#}");
    }

    #[test]
    fn a_shared_port_is_not_blocked_blindly_when_the_host_does_not_resolve() {
        let worker = at("aurcache.example.com", 443);
        assert!(
            scope(
                &Resolved {
                    endpoint: &worker,
                    addrs: &[],
                },
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn a_narrowed_rule_names_its_destination() {
        let args = rule_args(967, 443, Some(ip("192.0.2.1")));
        let d = args.iter().position(|a| a == "-d").unwrap();
        assert_eq!(args[d + 1], "192.0.2.1");
    }

    #[test]
    fn a_rule_naming_our_own_uid_is_refused() {
        assert!(check_not_self(967, 1000, "builder").is_ok());
        let err = check_not_self(1000, 1000, "aurcache").unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("own uid"), "{message}");
        assert!(message.contains("aurcache"), "{message}");
    }

    #[test]
    fn rule_names_uid_and_port_and_nothing_else() {
        assert_eq!(
            rule_args(967, 8083, None),
            [
                "OUTPUT",
                "-m",
                "owner",
                "--uid-owner",
                "967",
                "-p",
                "tcp",
                "--dport",
                "8083",
                "-j",
                "REJECT"
            ]
            .map(str::to_string)
            .to_vec()
        );
    }

    /// A fake `sudo` + `iptables` pair records invocations and answers `-C`
    /// from a state file, so `ensure_one_in` runs hermetically with a
    /// child-scoped `PATH` (this process's environment is never touched).
    /// Note `$2`, not `$1`: the stub runs as `sudo iptables …`, so the tool
    /// name itself occupies `$1`.
    fn fake_toolbox() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let state = dir.path().join("present");
        std::fs::write(
            dir.path().join("sudo"),
            format!(
                "#!/bin/sh\nexec \"{self_}/iptables\" \"$@\"\n",
                self_ = dir.path().display()
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("iptables"),
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{log}\"\n\
                 if [ \"$2\" = \"-C\" ]; then [ -f \"{state}\" ]; exit $?; fi\n\
                 touch \"{state}\"; exit 0\n",
                log = log.display(),
                state = state.display()
            ),
        )
        .unwrap();
        for tool in ["sudo", "iptables"] {
            let path = dir.path().join(tool);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[test]
    fn ensure_is_check_then_add_then_stable() {
        let toolbox = fake_toolbox();
        let bin = toolbox.path();
        // Child-scoped `PATH`: the fake `sudo`/`iptables` are found by the
        // stubbed runs only, and the real environment is never mutated.
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let path = Some(path.as_str());
        ensure_one_in("iptables", 967, 8083, None, path).unwrap();
        let first = std::fs::read_to_string(bin.join("calls.log")).unwrap();
        assert!(first.contains("-C OUTPUT"), "checks first:\n{first}");
        assert!(first.contains("-I OUTPUT"), "adds when absent:\n{first}");
        ensure_one_in("iptables", 967, 8083, None, path).unwrap();
        let second = std::fs::read_to_string(bin.join("calls.log")).unwrap();
        assert_eq!(
            second.matches("-I OUTPUT").count(),
            1,
            "no duplicate insert:\n{second}"
        );
        assert!(
            second.contains("--uid-owner 967"),
            "rule names the build uid:\n{second}"
        );
        assert!(
            second.contains("--dport 8083"),
            "rule names the worker port:\n{second}"
        );
    }
}
