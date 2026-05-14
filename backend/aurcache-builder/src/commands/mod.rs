use aurcache_db::packages::SourceData;
use std::path::Path;

pub fn build_build_command(
    source_data: &SourceData,
    pkgbase: &str,
    build_flags: &str,
    container_build_dir: &Path,
) -> String {
    match source_data {
        SourceData::Aur { .. } => {
            let rpc_url = std::env::var("AUR_RPC_URL")
                .unwrap_or_else(|_| "https://aur.archlinux.org/rpc/v5".to_string());
            let snapshot_url = aurcache_deps::snapshot_url(&rpc_url, pkgbase);
            format!(
                "sudo pacman -Syu --noconfirm --noprogressbar --color never && \
                 mkdir -p {build_dir} && cd {build_dir} && \
                 curl -sL '{snapshot_url}' | tar xz && \
                 cd {pkgbase} && \
                 makepkg -s {build_flags}",
                build_dir = container_build_dir.display(),
            )
        }
        SourceData::Git { .. } => {
            let self_update = "pacman -Syu --noconfirm --noprogressbar --color never";
            format!(
                "sudo chmod -R 1777 /tmp && {self_update} && cd /tmp && makepkg -s {build_flags}"
            )
        }
        SourceData::Upload { .. } => {
            todo!("unpack zip and store it in build container dir")
        }
    }
}

pub fn wrap_with_makepkg_config(
    makepkg_config: &str,
    makepkg_config_path: &str,
    pacman_config: &str,
    build_cmd: &str,
) -> String {
    format!(
        "cat <<'__AURCACHE_MAKEPKG_EOF__' > {makepkg_config_path}\n{makepkg_config}\n__AURCACHE_MAKEPKG_EOF__\n\
         cat <<'__AURCACHE_PACMAN_EOF__' | sudo tee /etc/pacman.conf >/dev/null\n{pacman_config}\n__AURCACHE_PACMAN_EOF__\n\
         {build_cmd}"
    )
}
