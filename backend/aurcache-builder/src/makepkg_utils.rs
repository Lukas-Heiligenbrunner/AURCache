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

/// Optional pacman.conf override. Returns `Some(content)` to overwrite
/// `/etc/pacman.conf` inside the build container, or `None` to leave the
/// builder image's default in place.
pub async fn read_pacman_config(db: &DatabaseConnection, pkg_id: i32) -> Option<String> {
    let user_conf = ApplicationSettings::get::<String>(Setting::PacmanConf, Some(pkg_id), db)
        .await
        .value;
    if user_conf.trim().is_empty() {
        None
    } else {
        Some(user_conf)
    }
}
