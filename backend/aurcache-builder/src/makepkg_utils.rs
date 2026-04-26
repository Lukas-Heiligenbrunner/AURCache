use std::path::Path;

pub fn create_makepkg_config(pkgdest_dir_base: &Path) -> anyhow::Result<(String, String)> {
    let makepkg_config = format!(
        "
MAKEFLAGS=-j$(nproc)
PKGDEST={pkgdest_dir_base}
",
        pkgdest_dir_base = pkgdest_dir_base.display()
    );
    let makepkg_config_path = "/var/ab/.config/pacman/makepkg.conf";
    Ok((makepkg_config, makepkg_config_path.to_string()))
}
