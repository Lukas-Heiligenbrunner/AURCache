pub fn create_makepkg_config(
    _name: String,
    _build_dir_base: &str,
) -> anyhow::Result<(String, String)> {
    let makepkg_config = "MAKEFLAGS=-j$(nproc)".to_string();
    let makepkg_config_path = "/var/ab/.config/pacman/makepkg.conf";
    Ok((makepkg_config, makepkg_config_path.to_string()))
}
