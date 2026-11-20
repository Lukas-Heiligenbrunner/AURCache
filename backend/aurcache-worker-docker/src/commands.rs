//! The shell command run inside the builder container.
//!
//! Carried over from the pre-worker builder so that packages built through the
//! compatibility path are built the same way they were before the upgrade.

use std::path::Path;

use aurcache_common::shell::quote_argument;

fn shell_join_args(args: &[String]) -> String {
    // Quoted whole: each element is already one argv entry, and splitting on
    // whitespace mangles exactly the flags that need quoting most (a value
    // containing a space becomes two shell words with a new meaning).
    args.iter()
        .map(|a| quote_argument(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Import the `validpgpkeys` a PKGBUILD declares, skipping keys already held.
///
/// The keyserver is the shared executor default, not a literal: the chroot
/// executor reads the same default from its settings, and two spellings of
/// the URL would drift.
fn fetch_required_pgp_keys_cmd() -> String {
    format!(
        "pgp_keys=\"$(if [ -f .SRCINFO ]; then \
         sed -n 's/^[[:space:]]*validpgpkeys[[:space:]]*=[[:space:]]*//p' .SRCINFO; \
     else \
         makepkg --printsrcinfo | sed -n 's/^[[:space:]]*validpgpkeys[[:space:]]*=[[:space:]]*//p'; \
     fi)\" && \
     if [ -n \"$pgp_keys\" ]; then \
         while IFS= read -r key; do \
             [ -n \"$key\" ] || continue; \
             if ! gpg --batch --list-keys \"$key\" >/dev/null 2>&1; then \
                 gpg --batch --keyserver {} --recv-keys \"$key\"; \
             fi; \
         done <<< \"$pgp_keys\"; \
     fi",
        aurcache_worker_core::settings::DEFAULT_KEYSERVER
    )
}

/// Build the shell command that runs inside the builder container.
///
/// The source tree is already present at `{build_dir}/{pkgbase}` through the
/// bind mount, so this just enters it and runs makepkg.
///
/// `chmod -R a+w .` is needed because the sources are written by the worker
/// process, which is not the container's build user: makepkg rewrites PKGBUILD
/// in place for VCS packages (`pkgver()` at build time). The container is
/// ephemeral, so the loosened permissions do not outlive the build.
///
/// The container is brought up to date first, in two steps: every database
/// `/etc/pacman.conf` lists is synced, AURCache's repository included, and the
/// system is then upgraded reading only the databases `upgrade_conf` lists --
/// the same configuration without that repository. pacman acts on
/// `replaces=` only while upgrading, and `--noconfirm` accepts the
/// replacement, so an upgrade that could see the repository would install a
/// package built there in place of any it names that no earlier repository
/// has -- in an image, a package since dropped from the official ones.
/// makepkg's `-s` then installs dependencies from all of them through
/// `pacman -S`, which ignores `replaces=`. One sync, so the dependencies come
/// from the databases the system was upgraded against.
#[must_use]
pub fn build_build_command(
    pkgbase: &str,
    build_flags: &[String],
    build_dir: &Path,
    makepkg_conf: &str,
    upgrade_conf: &str,
) -> String {
    let build_dir = quote_argument(&build_dir.display().to_string());
    let conf = quote_argument(makepkg_conf);
    let quoted_pkgbase = quote_argument(pkgbase);
    let quoted_build_flags =
        shell_join_args(&aurcache_common::build_flags::for_makepkg(build_flags));

    let pacman = "sudo pacman --noconfirm --noprogressbar --color never";
    let self_update = format!(
        "{pacman} -Sy && {pacman} -Su --config {upgrade_conf}",
        upgrade_conf = quote_argument(upgrade_conf),
    );

    format!(
        "{self_update} && cd {build_dir}/{quoted_pkgbase} && sudo chmod -R a+w . && \
         export BUILDDIR=$(mktemp -d) && export SRCDEST=$(mktemp -d) && \
         {fetch_pgp_keys} && makepkg --config {conf} -s {build_flags}",
        fetch_pgp_keys = fetch_required_pgp_keys_cmd(),
        build_flags = quoted_build_flags,
    )
}

/// The job's makepkg assignments, then this builder's own: where packages go,
/// and a job per core.
///
/// Last, so they win over anything the operator's configuration set: the job
/// leaves both to the worker, and the packages are collected from `pkgdest`.
#[must_use]
pub fn with_layout(makepkg_conf: &str, pkgdest: &str) -> String {
    let mut out = makepkg_conf.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("PKGDEST={pkgdest}\nMAKEFLAGS=\"-j$(nproc)\"\n"));
    out
}

/// Prefix the build with the job's rendered `makepkg.conf`.
///
/// The written config `source`s `/etc/makepkg.conf` before applying the
/// server's settings, and is passed to `makepkg --config`.
///
/// That indirection is necessary because makepkg loads exactly *one* config
/// file — `${MAKEPKG_CONF:-/etc/makepkg.conf}` (see `load_makepkg_config`). It
/// does **not** read `~/.makepkg.conf`; that overlay behaviour no longer
/// exists. Writing the server's settings alone would therefore lose `PKGEXT`,
/// `SRCEXT` and the compression settings ("$PKGEXT does not contain a valid
/// package suffix"), while writing them to a home-directory path would be
/// ignored entirely — silently producing debug packages and putting the built
/// package somewhere the worker does not look.
///
/// `pacman.conf` and the mirrorlist are deliberately absent here: they are
/// bind-mounted over `/etc` instead of written in. The build user has
/// passwordless sudo for `pacman`, `pacman-key` and `chmod` alone, in every
/// builder image ever published -- `aurcache-builder-v2` and the
/// `ghcr.io/.../aurcache-builder:latest` a 0.5.0 deployment keeps pulling
/// alike -- so writing anywhere under `/etc`
/// fails with "a password is required" and takes the build with it.
#[must_use]
pub fn wrap_with_config(
    makepkg_config: &str,
    makepkg_config_path: &str,
    build_cmd: &str,
) -> String {
    format!(
        "printf 'source /etc/makepkg.conf\\n' > {makepkg_config_path}\n\
         printf '%s' {makepkg_config} >> {makepkg_config_path}\n\
         {build_cmd}",
        makepkg_config = quote_argument(makepkg_config),
        makepkg_config_path = quote_argument(makepkg_config_path),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_command_fetches_required_pgp_keys() {
        let cmd = build_build_command(
            "hello",
            &[
                "--noconfirm".to_string(),
                "--noprogressbar".to_string(),
                "--nocolor".to_string(),
            ],
            Path::new("/output/src"),
            "/output/makepkg.conf",
            "/output/pacman-upgrade.conf",
        );

        assert!(cmd.contains("validpgpkeys"));
        assert!(cmd.contains("gpg --batch --keyserver hkps://keyserver.ubuntu.com --recv-keys"));
        assert!(cmd.contains("makepkg --config /output/makepkg.conf -s --noconfirm"));
        assert!(cmd.contains("cd /output/src/hello"));
        assert!(cmd.contains("sudo chmod -R a+w ."));
        assert!(cmd.contains("BUILDDIR=$(mktemp -d)"));
    }

    /// makepkg takes the flags every build needs whether or not the server
    /// sent them, and once each.
    #[test]
    fn makepkg_gets_every_build_flag_once() {
        let build = |flags: &[&str]| {
            build_build_command(
                "hello",
                &flags.iter().map(ToString::to_string).collect::<Vec<_>>(),
                Path::new("/output/src"),
                "/output/makepkg.conf",
                "/output/pacman-upgrade.conf",
            )
        };
        let expected =
            "makepkg --config /output/makepkg.conf -s --noconfirm --noprogressbar --nocolor";
        assert!(build(&[]).ends_with(expected), "{}", build(&[]));
        let sent = build(&["--noconfirm", "--noprogressbar", "--nocolor", "--nocheck"]);
        assert!(sent.ends_with(&format!("{expected} --nocheck")), "{sent}");
    }

    /// The container syncs every database, then upgrades from the
    /// configuration without AURCache's repository, before anything else.
    /// No pacman call in the command both syncs and upgrades: one that did
    /// would upgrade with the repository in view, and act on a `replaces=`
    /// in it.
    #[test]
    fn the_upgrade_never_sees_the_repository() {
        let cmd = build_build_command(
            "hello",
            &["--noconfirm".to_string()],
            Path::new("/output/src"),
            "/output/makepkg.conf",
            "/output/pacman-upgrade.conf",
        );
        let pacman = "sudo pacman --noconfirm --noprogressbar --color never";
        assert!(
            cmd.starts_with(&format!(
                "{pacman} -Sy && {pacman} -Su --config /output/pacman-upgrade.conf && "
            )),
            "{cmd}"
        );
        for (index, _) in cmd.match_indices("pacman ") {
            let call = cmd[index..].split("&&").next().unwrap_or_default();
            let upgrades = call.split_whitespace().any(|word| {
                word.strip_prefix('-')
                    .filter(|letters| !letters.starts_with('-'))
                    .is_some_and(|letters| letters.contains('S') && letters.contains('u'))
            });
            assert!(
                !upgrades || call.contains("--config /output/pacman-upgrade.conf"),
                "an upgrade with the build's configuration: {call}"
            );
        }
    }

    /// A pkgbase is attacker-influenced (it is whatever the user asked to
    /// build), so it must never break out of its quoting. Asserting on the
    /// escaped text alone proves little — the escaped form still *contains*
    /// the dangerous characters — so hand the result to a real shell and check
    /// it comes back as one intact argument.
    #[test]
    fn hostile_pkgbase_survives_a_real_shell_as_one_argument() {
        for hostile in [
            "evil\'; rm -rf /; \'",
            "a$(touch /tmp/pwned)b",
            "x`id`y",
            "back\\slash",
        ] {
            let quoted = quote_argument(hostile);
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {quoted}"))
                .output()
                .expect("sh should run");
            assert!(out.status.success(), "shell rejected {quoted}");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                hostile,
                "quoting of {hostile:?} did not round-trip"
            );
        }
    }

    /// One argv entry stays one shell word: splitting a flag that carries a
    /// space (e.g. `--opt="a b"`) changes its meaning, which is exactly what
    /// the quoting is there to prevent.
    #[test]
    fn a_flag_with_a_space_survives_as_one_word() {
        let joined = shell_join_args(&["--opt=a b".to_string(), "--flag".to_string()]);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '<%s>' {joined}"))
            .output()
            .expect("sh should run");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "<--opt=a b><--flag>");
    }

    /// The builder's own settings come after the operator's, so packages land
    /// where the worker collects them whatever the operator wrote.
    #[test]
    fn the_builder_sets_where_packages_go_last() {
        let conf = with_layout("PKGDEST=/elsewhere\nPACKAGER='x'", "/output");
        assert!(conf.ends_with("PACKAGER='x'\nPKGDEST=/output\nMAKEFLAGS=\"-j$(nproc)\"\n"));
    }

    /// Only what the builder image permits may run under sudo: `pacman`,
    /// `pacman-key` and `chmod`. The image of 0.5.0, still what an unchanged
    /// compose file pulls, grants nothing more, and any other `sudo` fails the
    /// build with "a password is required".
    #[test]
    fn only_the_permitted_sudo_commands_are_used() {
        let build = build_build_command(
            "hello",
            &["--noconfirm".to_string()],
            Path::new("/output/src"),
            "/output/makepkg.conf",
            "/output/pacman-upgrade.conf",
        );
        let conf = wrap_with_config("MK", "/output/makepkg.conf", &build);
        // The system defaults must be pulled in first: makepkg loads exactly
        // one config file, so the server's settings alone would lose PKGEXT.
        assert!(conf.contains("source /etc/makepkg.conf"));
        let sourced = conf.find("source /etc/makepkg.conf").unwrap();
        let overrides = conf.find("printf '%s' MK").unwrap();
        assert!(
            sourced < overrides,
            "defaults must be sourced before overrides"
        );
        assert!(!conf.contains("/etc/pacman"));
        for (index, _) in conf.match_indices("sudo ") {
            let command = conf[index + "sudo ".len()..].split_whitespace().next();
            assert!(
                matches!(command, Some("pacman" | "pacman-key" | "chmod")),
                "unpermitted sudo use: {}",
                &conf[index..]
            );
        }
    }
}
