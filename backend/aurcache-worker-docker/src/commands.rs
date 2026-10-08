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
#[must_use]
pub fn build_build_command(
    pkgbase: &str,
    build_flags: &[String],
    build_dir: &Path,
    makepkg_conf: &str,
) -> String {
    let build_dir = quote_argument(&build_dir.display().to_string());
    let conf = quote_argument(makepkg_conf);
    let quoted_pkgbase = quote_argument(pkgbase);
    let quoted_build_flags = shell_join_args(build_flags);

    let self_update = "sudo pacman -Syu --noconfirm --noprogressbar --color never";

    format!(
        "{self_update} && cd {build_dir}/{quoted_pkgbase} && sudo chmod -R a+w . && \
         export BUILDDIR=$(mktemp -d) && export SRCDEST=$(mktemp -d) && \
         {fetch_pgp_keys} && makepkg --config {conf} -s {build_flags}",
        fetch_pgp_keys = fetch_required_pgp_keys_cmd(),
        build_flags = quoted_build_flags,
    )
}

/// Prefix the build with the job's rendered `makepkg.conf` / `pacman.conf`.
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
/// The mirrorlist is deliberately absent here: it is bind-mounted over
/// `/etc/pacman.d/mirrorlist` instead of written in. The builder image grants
/// the build user passwordless sudo for exactly four commands — `pacman`,
/// `pacman-key`, `chmod`, and `tee /etc/pacman.conf` — so `sudo tee` on any
/// other path fails with "a password is required" and takes the build with it.
#[must_use]
pub fn wrap_with_config(
    makepkg_config: &str,
    makepkg_config_path: &str,
    pacman_config: &str,
    build_cmd: &str,
) -> String {
    format!(
        "printf 'source /etc/makepkg.conf\\n' > {makepkg_config_path}\n\
         printf '%s' {makepkg_config} >> {makepkg_config_path}\n\
         printf '%s' {pacman_config} | sudo tee /etc/pacman.conf >/dev/null\n\
         {build_cmd}",
        makepkg_config = quote_argument(makepkg_config),
        makepkg_config_path = quote_argument(makepkg_config_path),
        pacman_config = quote_argument(pacman_config),
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
        );

        assert!(cmd.contains("validpgpkeys"));
        assert!(cmd.contains("gpg --batch --keyserver hkps://keyserver.ubuntu.com --recv-keys"));
        assert!(cmd.contains("makepkg --config /output/makepkg.conf -s --noconfirm"));
        assert!(cmd.contains("cd /output/src/hello"));
        assert!(cmd.contains("sudo chmod -R a+w ."));
        assert!(cmd.contains("BUILDDIR=$(mktemp -d)"));
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

    /// Only the one sudo path the builder image actually permits may appear.
    /// The image grants passwordless sudo for `pacman`, `pacman-key`, `chmod`
    /// and `tee /etc/pacman.conf` only; any other `sudo` fails the build with
    /// "a password is required".
    #[test]
    fn only_the_permitted_sudo_commands_are_used() {
        let conf = wrap_with_config("MK", "/output/makepkg.conf", "PAC", "build");
        assert!(conf.contains("sudo tee /etc/pacman.conf"));
        // The system defaults must be pulled in first: makepkg loads exactly
        // one config file, so the server's settings alone would lose PKGEXT.
        assert!(conf.contains("source /etc/makepkg.conf"));
        let sourced = conf.find("source /etc/makepkg.conf").unwrap();
        let overrides = conf.find("printf '%s' MK").unwrap();
        assert!(
            sourced < overrides,
            "defaults must be sourced before overrides"
        );
        assert!(!conf.contains("/etc/pacman.d/mirrorlist"));
        for line in conf.lines().filter(|l| l.contains("sudo ")) {
            assert!(
                line.contains("sudo tee /etc/pacman.conf"),
                "unpermitted sudo use: {line}"
            );
        }
    }
}
