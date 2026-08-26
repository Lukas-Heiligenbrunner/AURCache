use std::path::Path;

/// Root directory inside the builder container where packages are written.
/// Corresponds to `PKGDEST` in makepkg.conf.
pub const CONTAINER_PKGDEST_DIR: &str = "/build";

/// Working directory inside the builder container where sources are unpacked.
pub const CONTAINER_BUILD_DIR: &str = "/build/src";

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn shell_join_args(args: &str) -> String {
    args.split_whitespace()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn fetch_required_pgp_keys_cmd() -> &'static str {
    "pgp_keys=\"$(if [ -f .SRCINFO ]; then \
         sed -n 's/^[[:space:]]*validpgpkeys[[:space:]]*=[[:space:]]*//p' .SRCINFO; \
     else \
         makepkg --printsrcinfo | sed -n 's/^[[:space:]]*validpgpkeys[[:space:]]*=[[:space:]]*//p'; \
     fi)\" && \
     if [ -n \"$pgp_keys\" ]; then \
         while IFS= read -r key; do \
             [ -n \"$key\" ] || continue; \
             if ! gpg --batch --list-keys \"$key\" >/dev/null 2>&1; then \
                 gpg --batch --keyserver hkps://keyserver.ubuntu.com --recv-keys \"$key\"; \
             fi; \
         done <<< \"$pgp_keys\"; \
     fi"
}

/// Build the shell command that runs inside the builder container.
///
/// The source archive is uploaded as a tar (extracted by Docker) to
/// `container_build_dir` before the container starts, so this command just
/// changes into the extracted pkgbase directory and runs makepkg.
///
/// We run `sudo chmod -R a+w .` because Docker's `upload_to_container`
/// extracts the tar as root, so all source files (including PKGBUILD) are
/// owned by root.  makepkg needs to modify PKGBUILD in-place for VCS
/// packages (e.g.  git packages where `pkgver()` is evaluated at build
/// time), so we make everything writable before invoking makepkg.  The
/// container is ephemeral so permissions don't leak.
pub fn build_build_command(pkgbase: &str, build_flags: &str, container_build_dir: &Path) -> String {
    let build_dir = shell_quote(&container_build_dir.display().to_string());
    let quoted_pkgbase = shell_quote(pkgbase);
    let quoted_build_flags = shell_join_args(build_flags);

    let self_update = "sudo pacman -Syu --noconfirm --noprogressbar --color never";

    format!(
        "{self_update} && cd {build_dir}/{quoted_pkgbase} && sudo chmod -R a+w . && export BUILDDIR=$(mktemp -d) && export SRCDEST=$(mktemp -d) && {fetch_pgp_keys} && makepkg -s {build_flags}",
        fetch_pgp_keys = fetch_required_pgp_keys_cmd(),
        build_flags = quoted_build_flags,
    )
}

pub fn wrap_with_makepkg_config(
    makepkg_config: &str,
    makepkg_config_path: &str,
    pacman_config: &str,
    build_cmd: &str,
) -> String {
    format!(
        "printf '%s' {makepkg_config} > {makepkg_config_path}\n\
         printf '%s' {pacman_config} | sudo tee /etc/pacman.conf >/dev/null\n\
         {build_cmd}",
        makepkg_config = shell_quote(makepkg_config),
        makepkg_config_path = shell_quote(makepkg_config_path),
        pacman_config = shell_quote(pacman_config),
    )
}

#[cfg(test)]
mod tests {
    use super::build_build_command;
    use std::path::Path;

    #[test]
    fn build_command_fetches_required_pgp_keys() {
        let cmd = build_build_command(
            "hello",
            "--noconfirm --noprogressbar --nocolor",
            Path::new("/build/src"),
        );

        assert!(cmd.contains("validpgpkeys"));
        assert!(cmd.contains("gpg --batch --keyserver hkps://keyserver.ubuntu.com --recv-keys"));
        assert!(cmd.contains("makepkg -s '--noconfirm' '--noprogressbar' '--nocolor'"));
        assert!(cmd.contains("cd '/build/src'/'hello'"));
        assert!(cmd.contains("sudo chmod -R a+w ."));
        assert!(cmd.contains("BUILDDIR=$(mktemp -d)"));
    }
}
