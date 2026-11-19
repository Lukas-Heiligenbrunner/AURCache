//! Cloning a package source over SSH, against a real `sshd`.
//!
//! The server runs as this user on a free loopback port and authenticates
//! this user only, so nothing here needs root. It needs OpenSSH's `sshd` and
//! `ssh-keygen`, and `git-upload-pack` on this user's `PATH` for the server
//! side of the fetch. Without them each test says it was skipped and why --
//! except in CI, which installs them, and where a skip would hide a broken
//! job: there it fails.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aurcache_utils::git::{Access, checkout_or_fetch_repo_ref, ls_remote};
use aurcache_utils::ssh::{HostKeySource, KeySource, SshAccess, SshConfig};

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

/// Skip a test this machine cannot run, saying why; fail it in CI.
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

    /// This server's host key, as a `known_hosts` line.
    fn known_hosts_line(&self) -> String {
        let public = std::fs::read_to_string(self.dir.path().join("host_ed25519.pub")).unwrap();
        format!("[127.0.0.1]:{} {}", self.port, public.trim())
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

/// A bare repository holding one PKGBUILD.
fn bare_repo(dir: &Path, pkgver: &str) -> PathBuf {
    let work = dir.join("work");
    let repo = git2::Repository::init(&work).unwrap();
    std::fs::write(work.join("PKGBUILD"), format!("pkgver={pkgver}\n")).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("PKGBUILD")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, pkgver, &tree, &[])
        .unwrap();
    let bare = dir.join("pkg.git");
    git2::build::RepoBuilder::new()
        .bare(true)
        .clone(work.to_str().unwrap(), &bare)
        .unwrap();
    bare
}

fn public_key(access: &SshAccess) -> String {
    use aurcache_common::api::ssh::{DEFAULT_KEY, SshKey};
    match access.info().key(DEFAULT_KEY).cloned() {
        Some(SshKey::Ready { public_key, .. }) => public_key,
        Some(SshKey::Unavailable { reason }) => panic!("{reason}"),
        None => panic!("no default key"),
    }
}

/// First contact records the host; the key is refused until the remote
/// knows it, then the clone and a later fetch work; a host presenting a
/// different key is refused.
#[test]
fn a_package_source_is_cloned_over_ssh_with_the_servers_key() {
    let Some(sshd) = Sshd::start() else { return };
    let state = tempfile::tempdir().unwrap();
    let known_hosts = state.path().join("ssh/known_hosts");
    let access = SshAccess::new(SshConfig {
        key: KeySource::Generated(state.path().join("ssh/id_ed25519")),
        passphrase_file: None,
        hosts: HostKeySource::TrustOnFirstUse(known_hosts.clone()),
    });
    let repo = bare_repo(state.path(), "1.0");
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

    // Another host answering on the same address.
    let other = PathBuf::from(format!("{}.other", known_hosts.display()));
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&other)
        .status()
        .unwrap();
    assert!(made.success());
    let impostor =
        std::fs::read_to_string(PathBuf::from(format!("{}.pub", other.display()))).unwrap();
    std::fs::write(
        &known_hosts,
        format!("[127.0.0.1]:{} {}", sshd.port, impostor.trim()),
    )
    .unwrap();
    let changed = ls_remote(&url, "HEAD", Access::Server(&access)).unwrap_err();
    let changed = format!("{changed:#}");
    assert!(changed.contains("records another"), "{changed}");
}

/// With an operator's `known_hosts`, an unlisted host is refused and
/// nothing is added; listed, it is trusted.
#[test]
fn a_pinned_known_hosts_is_the_whole_list() {
    let Some(sshd) = Sshd::start() else { return };
    let state = tempfile::tempdir().unwrap();
    let pinned = state.path().join("known_hosts");
    std::fs::write(&pinned, "").unwrap();
    let access = SshAccess::new(SshConfig {
        key: KeySource::Generated(state.path().join("ssh/id_ed25519")),
        passphrase_file: None,
        hosts: HostKeySource::Pinned(pinned.clone()),
    });
    sshd.authorize(&public_key(&access));
    let repo = bare_repo(state.path(), "1.0");
    let url = sshd.url(&repo);

    let refused = ls_remote(&url, "HEAD", Access::Server(&access)).unwrap_err();
    let refused = format!("{refused:#}");
    assert!(
        refused.contains("is not in AURCACHE_SSH_KNOWN_HOSTS"),
        "{refused}"
    );
    assert!(
        refused.contains(&format!("ssh-keyscan -p {} 127.0.0.1", sshd.port)),
        "{refused}"
    );
    assert_eq!(std::fs::read_to_string(&pinned).unwrap(), "");
    // libgit2 reaches `SSH://` over SSH too, so it is judged the same way
    // rather than connected to with libgit2's defaults.
    let shouted = url.replacen("ssh://", "SSH://", 1);
    let refused = ls_remote(&shouted, "HEAD", Access::Server(&access)).unwrap_err();
    let refused = format!("{refused:#}");
    assert!(
        refused.contains("is not in AURCACHE_SSH_KNOWN_HOSTS"),
        "{refused}"
    );

    std::fs::write(&pinned, format!("{}\n", sshd.known_hosts_line())).unwrap();
    ls_remote(&url, "HEAD", Access::Server(&access)).unwrap();
    ls_remote(&shouted, "HEAD", Access::Server(&access)).unwrap();
}

/// git's bracketed scp form, `[user@host:port]:path`, names a port, and
/// libgit2 connects to it. The host key is judged as that port's: an entry
/// for the host on port 22 does not vouch for it, and first use records it
/// under `[host]:port`.
#[test]
fn a_port_in_the_bracketed_scp_form_is_the_one_judged() {
    let Some(sshd) = Sshd::start() else { return };
    let state = tempfile::tempdir().unwrap();
    let key = state.path().join("ssh/id_ed25519");
    let pinned = state.path().join("known_hosts");
    let host_key = std::fs::read_to_string(sshd.dir.path().join("host_ed25519.pub")).unwrap();
    // The right key, under the name of port 22 only.
    std::fs::write(&pinned, format!("127.0.0.1 {}\n", host_key.trim())).unwrap();
    let access = SshAccess::new(SshConfig {
        key: KeySource::Generated(key.clone()),
        passphrase_file: None,
        hosts: HostKeySource::Pinned(pinned.clone()),
    });
    sshd.authorize(&public_key(&access));
    let repo = bare_repo(state.path(), "1.0");
    let url = format!("[{}@127.0.0.1:{}]:{}", sshd.user, sshd.port, repo.display());

    let refused = ls_remote(&url, "HEAD", Access::Server(&access)).unwrap_err();
    let refused = format!("{refused:#}");
    assert!(
        refused.contains(&format!(
            "[127.0.0.1]:{} is not in AURCACHE_SSH_KNOWN_HOSTS",
            sshd.port
        )),
        "{refused}"
    );

    // Listed under its port's name, it is trusted.
    std::fs::write(&pinned, format!("{}\n", sshd.known_hosts_line())).unwrap();
    ls_remote(&url, "HEAD", Access::Server(&access)).unwrap();

    let first_use = state.path().join("first-use/known_hosts");
    let access = SshAccess::new(SshConfig {
        key: KeySource::Generated(key),
        passphrase_file: None,
        hosts: HostKeySource::TrustOnFirstUse(first_use.clone()),
    });
    ls_remote(&url, "HEAD", Access::Server(&access)).unwrap();
    let recorded = std::fs::read_to_string(&first_use).unwrap();
    assert!(
        recorded.starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", sshd.port)),
        "{recorded}"
    );
}

/// A supplied key protected by a passphrase is opened with the one in
/// `AURCACHE_GIT_SSH_KEY_PASSPHRASE_FILE`, by libssh2 itself.
#[test]
fn a_passphrase_protected_key_clones_with_its_passphrase_file() {
    let Some(sshd) = Sshd::start() else { return };
    let state = tempfile::tempdir().unwrap();
    let key = state.path().join("deploy_key");
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "correct horse", "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(made.success());
    let passphrase = state.path().join("passphrase");
    std::fs::write(&passphrase, "correct horse\n").unwrap();
    let pinned = state.path().join("known_hosts");
    std::fs::write(&pinned, format!("{}\n", sshd.known_hosts_line())).unwrap();

    let access = SshAccess::new(SshConfig {
        key: KeySource::Provided(key.clone()),
        passphrase_file: Some(passphrase.clone()),
        hosts: HostKeySource::Pinned(pinned),
    });
    sshd.authorize(&public_key(&access));
    let repo = bare_repo(state.path(), "1.0");
    let url = sshd.url(&repo);
    let checkout = state.path().join("checkout");
    checkout_or_fetch_repo_ref(&url, "HEAD", &checkout, Access::Server(&access)).unwrap();
    assert!(
        std::fs::read_to_string(checkout.join("PKGBUILD"))
            .unwrap()
            .contains("1.0")
    );

    // The wrong one is the reason, before anything connects.
    std::fs::write(&passphrase, "battery staple\n").unwrap();
    let wrong = SshAccess::new(SshConfig {
        key: KeySource::Provided(key),
        passphrase_file: Some(passphrase),
        hosts: HostKeySource::Pinned(state.path().join("known_hosts")),
    });
    let error = ls_remote(&url, "HEAD", Access::Server(&wrong)).unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("does not open"), "{error}");
}
