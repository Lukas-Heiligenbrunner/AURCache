pub fn create_makepkg_config(
    name: String,
    build_dir_base: &str,
) -> anyhow::Result<(String, String)> {
    let makepkg_config = format!(
        "\
MAKEFLAGS=-j$(nproc)
PKGDEST={}/{}",
        build_dir_base, name
    );
    let makepkg_config_path = "/var/ab/.config/pacman/makepkg.conf";
    Ok((makepkg_config, makepkg_config_path.to_string()))
}
