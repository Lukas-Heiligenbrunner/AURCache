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
            let base_url = std::env::var("AUR_RPC_URL")
                .map(|u| {
                    u.trim_end_matches('/')
                        .trim_end_matches("/rpc/v5")
                        .trim_end_matches('/')
                        .to_string()
                })
                .unwrap_or_else(|_| "https://aur.archlinux.org".to_string());
            let snapshot_url = format!("{base_url}/cgit/aur.git/snapshot/{pkgbase}.tar.gz");
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
    build_cmd: &str,
) -> String {
    format!(
        "cat <<'__AURCACHE_MAKEPKG_EOF__' > {makepkg_config_path}\n{makepkg_config}\n__AURCACHE_MAKEPKG_EOF__\n\
         {build_cmd}"
    )
}
