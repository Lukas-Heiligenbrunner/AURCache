use crate::settings::{self, key};
use sea_orm::DatabaseConnection;
use std::path::Path;

/// Canonical mirrorlist locations, re-exported from `aurcache-deps`.
///
/// They are defined there rather than here because `aurcache-db`'s
/// dependency-backfill migration builds an `AurClient`, so `aurcache-deps`
/// cannot depend on this crate. Re-exporting keeps one import path for
/// everyone else.
pub use aurcache_deps::paths::{
    mirrorlist_dir, mirrorlist_file_name, mirrorlist_path, native_arch, shared_mirrorlist_path,
};

/// The assignments a build applies over its worker's own makepkg.conf: the
/// `makepkg_conf` setting's `user_conf`, then the server's own.
///
/// The server's come last so the user cannot accidentally override them:
/// without a valid PACKAGER the generated `desc` file cannot be parsed by
/// libalpm. Where packages go (`PKGDEST`) and how many jobs a build runs
/// (`MAKEFLAGS`) are the worker's to set, after these: only it knows its
/// layout and its CPU limit.
///
/// `OPTIONS+=(!debug)` suppresses makepkg's split `<pkgname>-debug` packages.
/// Appended to the array rather than assigned: an assignment replaces the
/// whole of it, and makepkg reads an option the array does not mention as off
/// -- `strip`, `zipman` and `purge` with it. The last mention of an option is
/// the one that counts, so this wins over the chroot's `debug` and the user's.
/// Arch's stock `makepkg.conf` enables `debug`, so a worker on distro defaults
/// emits one per package. AURCache serves a single flat repo per architecture,
/// so those would show up in `pacman -Ss` alongside real packages — Arch itself
/// keeps them out of `core`/`extra` and ships them in separate opt-in `*-debug`
/// repos. Publishing debug symbols would mean a second repo, not extra entries
/// in this one.
fn makepkg_config(user_conf: &str) -> String {
    let mut config = String::new();
    if !user_conf.trim().is_empty() {
        config.push_str(user_conf);
        if !config.ends_with('\n') {
            config.push('\n');
        }
    }
    config.push_str("PACKAGER='AURCache <aurcache@localhost>'\nOPTIONS+=(!debug)\n");
    config
}

/// The standard pacman.conf written inside a build container.
///
/// `DisableSandbox`: pacman 7's Landlock download sandbox needs syscalls that
/// some runtimes block or do not implement (older seccomp profiles, qemu
/// emulation), and aborts every `pacman -Sy` where they are unavailable.
/// Disabling it keeps builds working on any host; it only relaxes pacman's
/// own download isolation, not the surrounding `makechrootpkg` chroot.
pub fn base_pacman_config() -> String {
    "[options]\nDisableSandbox\nSigLevel = Never\nHoldPkg = pacman glibc\nArchitecture = auto\n\n\
     [core]\nInclude = /etc/pacman.d/mirrorlist\n\n\
     [extra]\nInclude = /etc/pacman.d/mirrorlist\n\n\
     [multilib]\nInclude = /etc/pacman.d/mirrorlist\n"
        .to_string()
}

/// The pacman.conf written inside the build container, from the `pacman_conf`
/// setting's `user_conf`: that in place of the standard one, when it is set.
///
/// No `[repo]` section is emitted: the worker appends one rendered from the
/// template it received at registration, because only the worker knows which
/// address it reaches this server on.
fn pacman_config(user_conf: &str) -> String {
    if user_conf.trim().is_empty() {
        base_pacman_config()
    } else {
        format!("[options]\nDisableSandbox\n{user_conf}")
    }
}

/// Resolve the mirrorlist content AURCache holds for a given architecture.
///
/// Reads `mirrorlist_dir/mirrorlist.<arch>`; a missing or empty file yields
/// `None` (the worker then falls back to its image's built-in mirrorlist),
/// never an error.
///
/// No architecture is special-cased: an arch is supported exactly when its file
/// exists. Only `x86_64` is written today, so every other arch returns `None` on
/// its own, and adding one later needs no change here.
pub async fn mirrorlist_for(arch: &str, mirrorlist_dir: &Path) -> Option<String> {
    let path = mirrorlist_dir.join(mirrorlist_file_name(arch));
    match tokio::fs::read_to_string(&path).await {
        Ok(content) if !content.trim().is_empty() => Some(content),
        _ => None,
    }
}

/// The configuration a job carries for its worker's chroot.
pub struct JobConfig {
    pub makepkg_conf: String,
    /// Without a `[repo]` section: the worker appends one for the host it
    /// reaches this server on.
    pub pacman_conf: String,
}

/// Assemble the self-contained build configuration for one of `pkg_id`'s builds.
pub async fn build_job_config(db: &DatabaseConnection, pkg_id: i32) -> JobConfig {
    let (makepkg, pacman) = tokio::join!(
        settings::get(db, key::MAKEPKG_CONF, Some(pkg_id)),
        settings::get(db, key::PACMAN_CONF, Some(pkg_id)),
    );
    JobConfig {
        makepkg_conf: makepkg_config(&makepkg.value),
        pacman_conf: pacman_config(&pacman.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server never emits a `[repo]` section any more; the worker appends
    /// one for the host it actually reaches this server on.
    #[test]
    fn base_pacman_config_has_expected_markers_and_no_repo() {
        let conf = base_pacman_config();
        assert!(conf.contains("DisableSandbox"));
        assert!(conf.contains("SigLevel = Never"));
        assert!(conf.contains("Include = /etc/pacman.d/mirrorlist"));
        assert!(!conf.contains("[repo]"));
    }

    /// The build server forces this; a worker on Arch defaults would otherwise
    /// emit `<pkgname>-debug` packages into the single flat repo -- whatever
    /// the user's own configuration says.
    #[test]
    fn makepkg_config_disables_debug_packages() {
        for user_conf in ["", "OPTIONS=(debug)"] {
            let conf = makepkg_config(user_conf);
            assert!(conf.ends_with("OPTIONS+=(!debug)\n"), "got:\n{conf}");
        }
    }

    /// What makepkg ends up with, read the way it reads the array: the last
    /// mention of an option wins, and one never mentioned is off.
    fn options_after(conf: &str) -> Vec<String> {
        let script = format!(
            "OPTIONS=(strip docs !libtool !staticlibs emptydirs zipman purge debug !lto)\n\
             nproc() {{ echo 1; }}\n{conf}\nprintf '%s\\n' \"${{OPTIONS[@]}}\""
        );
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .output()
            .expect("running bash");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn enabled(options: &[String], option: &str) -> bool {
        options
            .iter()
            .rev()
            .find(|o| o.trim_start_matches('!') == option)
            .is_some_and(|o| !o.starts_with('!'))
    }

    /// Turning `debug` off must leave the rest of the chroot's options alone:
    /// assigning the array instead built every package unstripped.
    #[test]
    fn makepkg_config_keeps_the_other_options() {
        let options = options_after(&makepkg_config(""));
        assert!(!enabled(&options, "debug"), "{options:?}");
        for kept in ["strip", "zipman", "purge", "docs"] {
            assert!(enabled(&options, kept), "{kept} lost: {options:?}");
        }
    }

    #[tokio::test]
    async fn mirrorlist_for_unpopulated_arch_is_none() {
        let dir = std::env::temp_dir();
        assert!(mirrorlist_for("aarch64", &dir).await.is_none());
    }

    #[tokio::test]
    async fn mirrorlist_for_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(mirrorlist_for("x86_64", dir.path()).await.is_none());
    }

    /// The layout must be genuinely arch-keyed, not x86_64 with extra steps:
    /// writing another arch's file is all it should take to support it.
    #[tokio::test]
    async fn mirrorlist_for_reads_any_populated_arch() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("mirrorlist.aarch64"),
            "Server = https://arm.example/$repo/os/$arch\n",
        )
        .await
        .unwrap();

        assert!(mirrorlist_for("aarch64", dir.path()).await.is_some());
        // ...and it must not leak across architectures.
        assert!(mirrorlist_for("x86_64", dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn mirrorlist_for_reads_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("mirrorlist.x86_64"),
            "Server = https://mirror/$repo/os/$arch\n",
        )
        .await
        .unwrap();
        let content = mirrorlist_for("x86_64", dir.path()).await;
        assert!(content.unwrap().contains("Server = https://mirror"));
    }
}
