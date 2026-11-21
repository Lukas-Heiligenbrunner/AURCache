//! The parse profile through the real binary, run unprivileged: Landlock and
//! the read walk are all that keep a `--private` path unreadable there.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Reads each of `paths` and prints one line per path: its contents, or
/// `denied`.
fn read_each(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| {
            format!(
                "echo \"$(cat '{}' 2>/dev/null || echo denied)\"",
                path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The output of `script` run under the parse profile, or `None` when this
/// host cannot run an unprivileged parse at all -- which fails under CI, where
/// the kernel is expected to have what it needs.
fn parse_running(pkgbuild: &Path, private: &[PathBuf], script: &str) -> Option<String> {
    // SAFETY: no arguments, cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        // As root the parse changes user, and the permissions do the reading
        // policy instead; that is not what this covers.
        eprintln!("skipping: running as root");
        return None;
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_aurcache-sandbox"));
    command.arg("parse").arg("--pkgbuild").arg(pkgbuild);
    for path in private {
        command.arg("--private").arg(path);
    }
    let output = command
        .args(["--", "/bin/sh", "-c", script, "sh"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("needs Landlock and Linux 6.12") && std::env::var_os("CI").is_none() {
        eprintln!("skipping: {stderr}");
        return None;
    }
    assert!(output.status.success(), "the parse failed: {stderr}");
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A directory linked to another, `state -> srv`, with one private path
/// written through each: the walk reaches `srv` under its own name only, so
/// what is private stays so by either name while the rest of `srv` reads by
/// both.
#[test]
fn a_private_path_stays_unreadable_through_a_linked_directory() {
    let scratch = std::env::temp_dir().join(format!(
        "aurcache-sandbox-parse-linked-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(scratch.join("srv/db")).unwrap();
    std::fs::create_dir_all(scratch.join("pkg")).unwrap();
    let t = std::fs::canonicalize(&scratch).unwrap();
    std::fs::write(t.join("srv/db/secret"), "secret").unwrap();
    std::fs::write(t.join("srv/pub"), "pub").unwrap();
    std::os::unix::fs::symlink("srv", t.join("state")).unwrap();
    let pkgbuild = t.join("pkg/PKGBUILD");
    std::fs::write(&pkgbuild, "pkgname=demo\n").unwrap();

    let script = read_each(&[
        t.join("srv/db/secret"),
        t.join("state/db/secret"),
        t.join("srv/pub"),
        t.join("state/pub"),
    ]);
    let unconfined = Command::new("/bin/sh")
        .args(["-c", &script])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&unconfined.stdout),
        "secret\nsecret\npub\npub\n",
        "the fixture proves nothing unless it reads everything unconfined"
    );

    let confined = parse_running(
        &pkgbuild,
        &[t.join("srv/db"), t.join("state/build_logs")],
        &script,
    );
    std::fs::remove_dir_all(&t).ok();
    if let Some(confined) = confined {
        assert_eq!(confined, "denied\ndenied\npub\npub\n");
    }
}
