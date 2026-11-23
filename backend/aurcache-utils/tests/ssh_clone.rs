//! Cloning a package source over SSH, against a real `sshd`.
//!
//! The server runs as this user on a free loopback port and authenticates
//! this user only, so nothing here needs root. It needs OpenSSH's `sshd` and
//! `ssh-keygen`, and `git-upload-pack` on this user's `PATH` for the server
//! side of the fetch. Without them each test says it was skipped and why --
//! except in CI, which installs them, and where a skip would hide a broken
//! job: there it fails.

#[path = "../test_support/git.rs"]
mod test_git;

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_common::api::ssh::SshKey;
use aurcache_utils::git::{Access, checkout_or_fetch_repo_ref, ls_remote};
use aurcache_utils::ssh::{HostKeySource, KeySource, SshAccess};
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};

/// A throwaway `sshd`, stopped when dropped.
struct Sshd {
    dir: tempfile::TempDir,
    port: u16,
    user: String,
    child: Child,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Skip a test that this machine cannot run, saying why; fail it in CI.
fn skip(why: &str) -> Option<Sshd> {
    assert!(std::env::var_os("CI").is_none(), "{why}, which CI installs");
    eprintln!("skipped: {why}");
    None
}

impl Sshd {
    /// Start one, or `None` (having said why) when this machine cannot.
    fn start() -> Option<Self> {
        let Some(sshd) = ["/usr/sbin/sshd", "/usr/bin/sshd"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
        else {
            return skip("no sshd on this machine");
        };
        if !runs("git", &["--version"]) {
            return skip("no git to serve the repository");
        }
        let user = Command::new("id")
            .arg("-un")
            .output()
            .expect("id runs")
            .stdout;
        let user = String::from_utf8(user).unwrap().trim().to_string();

        let dir = tempfile::tempdir().unwrap();
        let host_key = dir.path().join("host_ed25519");
        if !runs(
            "ssh-keygen",
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                host_key.to_str().unwrap(),
            ],
        ) {
            return skip("no ssh-keygen to make a host key");
        }
        std::fs::write(dir.path().join("authorized_keys"), "").unwrap();

        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = dir.path().join("sshd_config");
        std::fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile none\n\
                 AuthorizedKeysFile {}\nStrictModes no\nUsePAM no\n\
                 PasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                 PubkeyAuthentication yes\nLogLevel ERROR\n",
                host_key.display(),
                dir.path().join("authorized_keys").display()
            ),
        )
        .unwrap();
        let child = Command::new(&sshd)
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("sshd starts");
        let sshd = Self {
            dir,
            port,
            user,
            child,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "sshd did not start listening");
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(sshd)
    }

    /// Let `public_key` log in.
    fn authorize(&self, public_key: &str) {
        std::fs::write(
            self.dir.path().join("authorized_keys"),
            format!("{public_key}\n"),
        )
        .unwrap();
    }

    /// The `ssh://` URL of `repo` on this server.
    fn url(&self, repo: &Path) -> String {
        format!(
            "ssh://{}@127.0.0.1:{}{}",
            self.user,
            self.port,
            repo.display()
        )
    }
}

fn runs(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn public_key(access: &SshAccess) -> String {
    match access.key() {
        SshKey::Ready { public_key, .. } => public_key,
        SshKey::Unavailable { reason } => panic!("{reason}"),
    }
}

/// First contact records the host; the key is refused until the remote
/// knows it, then the clone and a later fetch work, until the host's key
/// changes.
#[test]
fn a_package_source_is_cloned_over_ssh_with_the_servers_key() {
    let Some(sshd) = Sshd::start() else { return };
    let state = tempfile::tempdir().unwrap();
    let known_hosts = state.path().join("ssh/known_hosts");
    let access = SshAccess::new(
        KeySource::Generated(state.path().join("ssh/id_ed25519")),
        HostKeySource::TrustOnFirstUse(known_hosts.clone()),
        ActivityLog::discarding(),
    );
    let repo = state.path().join("pkg");
    test_git::create_repo(&repo, &[("PKGBUILD", "pkgver=1.0\n")]);
    let url = sshd.url(&repo);
    let checkout = state.path().join("checkout");

    let refused =
        checkout_or_fetch_repo_ref(&url, "HEAD", &checkout, Access::Server(&access)).unwrap_err();
    let refused = format!("{refused:#}");
    assert!(
        refused.contains("refused the server's SSH key"),
        "{refused}"
    );
    let recorded = std::fs::read_to_string(&known_hosts).unwrap();
    assert!(
        recorded.starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", sshd.port)),
        "{recorded}"
    );

    sshd.authorize(&public_key(&access));
    let _ = std::fs::remove_dir_all(&checkout);
    let first =
        checkout_or_fetch_repo_ref(&url, "HEAD", &checkout, Access::Server(&access)).unwrap();
    assert!(
        std::fs::read_to_string(checkout.join("PKGBUILD"))
            .unwrap()
            .contains("1.0")
    );
    // The reused checkout fetches through the same route.
    let again =
        checkout_or_fetch_repo_ref(&url, "HEAD", &checkout, Access::Server(&access)).unwrap();
    assert_eq!(first, again);
    assert_eq!(
        ls_remote(&url, "HEAD", Access::Server(&access)).unwrap(),
        first.to_string()
    );
    // Without the server's key, an SSH remote is not even tried.
    assert!(ls_remote(&url, "HEAD", Access::Anonymous).is_err());

    // A changed host key stops libgit2, and the refusal keeps its reason.
    let impostor = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let impostor = impostor.public_key().to_openssh().unwrap();
    std::fs::write(
        &known_hosts,
        format!("[127.0.0.1]:{} {impostor}\n", sshd.port),
    )
    .unwrap();
    let changed = ls_remote(&url, "HEAD", Access::Server(&access)).unwrap_err();
    let changed = format!("{changed:#}");
    assert!(changed.contains("intercepting"), "{changed}");
}
