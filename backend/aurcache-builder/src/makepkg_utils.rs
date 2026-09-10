use aurcache_types::settings::{ApplicationSettings, Setting};
use aurcache_utils::settings::general::SettingsTraits;
use sea_orm::DatabaseConnection;
use std::path::Path;

/// Build the makepkg.conf for a build.
///
/// User-provided content (from the `makepkg_conf` setting) is written first.
/// PKGDEST, MAKEFLAGS, and PACKAGER are always appended at the end so the
/// user cannot accidentally override them — without the right PKGDEST the
/// build can't be collected from the shared mount, and without a valid
/// PACKAGER the generated `desc` file cannot be parsed by libalpm.
///
/// Pass `None` for `db_ctx` when no database is available (e.g. the
/// test-builder binary); user config is then skipped.
pub async fn create_makepkg_config(
    db_ctx: Option<(&DatabaseConnection, i32)>,
    pkgdest_dir_base: &Path,
) -> anyhow::Result<(String, String)> {
    let mut config = String::new();

    if let Some((db, pkg_id)) = db_ctx {
        let user_conf = ApplicationSettings::get::<String>(Setting::MakepkgConf, Some(pkg_id), db)
            .await
            .value;
        if !user_conf.trim().is_empty() {
            config.push_str(&user_conf);
            if !config.ends_with('\n') {
                config.push('\n');
            }
        }
    }

    config.push_str(&format!(
        "MAKEFLAGS=-j$(nproc)\nPKGDEST={}\nPACKAGER='AURCache <aurcache@localhost>'\n",
        pkgdest_dir_base.display()
    ));

    let makepkg_config_path = "/var/ab/.config/pacman/makepkg.conf";
    Ok((config, makepkg_config_path.to_string()))
}

/// Generate the standard pacman.conf written inside a build container.
///
/// When `aurcache_repo_url` is `Some`, a `[repo]` section pointing at the
/// AURCache package server is appended so makepkg can resolve previously built
/// packages.  Pass `None` for standalone builds (e.g. the test-builder) where
/// no AURCache server is running.
pub fn base_pacman_config(aurcache_repo_url: Option<&str>) -> String {
    let base = "[options]\nDisableSandbox\nSigLevel = Never\nHoldPkg = pacman glibc\nArchitecture = auto\n\n\
                [core]\nInclude = /etc/pacman.d/mirrorlist\n\n\
                [extra]\nInclude = /etc/pacman.d/mirrorlist\n\n\
                [multilib]\nInclude = /etc/pacman.d/mirrorlist\n";
    match aurcache_repo_url {
        Some(url) => format!("{base}\n[repo]\nSigLevel = Never\nServer = {url}/$arch\n"),
        None => base.to_string(),
    }
}

/// Build the pacman.conf written inside the build container.
///
/// User-provided content replaces the stock repo sections but still gets the
/// AURCache repo appended so makepkg can resolve previously built packages.
pub async fn create_pacman_config(
    db: &DatabaseConnection,
    pkg_id: i32,
    aurcache_repo_url: &str,
) -> String {
    let user_conf = ApplicationSettings::get::<String>(Setting::PacmanConf, Some(pkg_id), db)
        .await
        .value;

    if user_conf.trim().is_empty() {
        base_pacman_config(Some(aurcache_repo_url))
    } else {
        let repo_conf = format!("\n[repo]\nSigLevel = Never\nServer = {aurcache_repo_url}/$arch\n");
        format!("[options]\nDisableSandbox\n{user_conf}{repo_conf}")
    }
}
