use aurcache_types::settings::{ApplicationSettings, Setting};
use aurcache_utils::settings::general::SettingsTraits;
use sea_orm::DatabaseConnection;
use std::path::Path;

/// Build the makepkg.conf for a build.
///
/// User-provided content (from the `makepkg_conf` setting) is written first.
/// PKGDEST and MAKEFLAGS are always appended at the end so the user cannot
/// accidentally override them — without the right PKGDEST the build can't be
/// collected from the shared mount.
pub async fn create_makepkg_config(
    db: &DatabaseConnection,
    pkg_id: i32,
    pkgdest_dir_base: &Path,
) -> anyhow::Result<(String, String)> {
    let user_conf = ApplicationSettings::get::<String>(Setting::MakepkgConf, Some(pkg_id), db)
        .await
        .value;

    let mut config = String::new();
    if !user_conf.trim().is_empty() {
        config.push_str(&user_conf);
        if !config.ends_with('\n') {
            config.push('\n');
        }
    }
    config.push_str(&format!(
        "MAKEFLAGS=-j$(nproc)\nPKGDEST={}\n",
        pkgdest_dir_base.display()
    ));

    let makepkg_config_path = "/var/ab/.config/pacman/makepkg.conf";
    Ok((config, makepkg_config_path.to_string()))
}

/// Build a minimal makepkg.conf without a database connection.
/// Suitable for the test-builder binary and other contexts where
/// no DB is available.
pub fn create_makepkg_config_minimal(pkgdest_dir_base: &Path) -> (String, String) {
    let config = format!(
        "MAKEFLAGS=-j$(nproc)\nPKGDEST={}\n",
        pkgdest_dir_base.display()
    );
    let makepkg_config_path = "/var/ab/.config/pacman/makepkg.conf";
    (config, makepkg_config_path.to_string())
}

/// Build the pacman.conf written inside the build container.
///
/// User-provided content replaces the stock repo sections but still gets the
/// AURCache repo appended so makepkg can resolve previously built packages.
pub async fn create_pacman_config(
    db: &DatabaseConnection,
    pkg_id: i32,
    aurcache_repo_mount: &str,
) -> String {
    let user_conf = ApplicationSettings::get::<String>(Setting::PacmanConf, Some(pkg_id), db)
        .await
        .value;
    let repo_conf =
        format!("\n[repo]\nSigLevel = Never\nServer = file://{aurcache_repo_mount}/$arch\n");

    if user_conf.trim().is_empty() {
        format!(
            "[options]\nDisableSandbox\nSigLevel = Never\nHoldPkg = pacman glibc\nArchitecture = auto\n\n\
             [core]\nInclude = /etc/pacman.d/mirrorlist\n\n\
             [extra]\nInclude = /etc/pacman.d/mirrorlist\n\n\
             [multilib]\nInclude = /etc/pacman.d/mirrorlist\n\n\
             {repo_conf}"
        )
    } else {
        format!("[options]\nDisableSandbox\n{user_conf}{repo_conf}")
    }
}
