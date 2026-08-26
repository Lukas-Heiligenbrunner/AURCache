use aurcache_builder::commands::{CONTAINER_BUILD_DIR, CONTAINER_PKGDEST_DIR};
use aurcache_builder::docker::mirrorlist_mount;
use aurcache_builder::makepkg_utils::{base_pacman_config, create_makepkg_config};
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    AttachContainerOptions, CreateContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use futures::StreamExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let package = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "hello".to_string());
    let builder_image = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "aurcache-builder:test".to_string());
    let build_flags = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "--noconfirm --noprogressbar --nocolor".to_string());

    println!("=== Testing builder image: {builder_image} ===");
    println!("Building package: {package}");
    println!("Build flags: {build_flags}");

    let docker = aurcache_builder::build::Builder::establish_docker_connection().await?;

    // Build the Docker image.  We still use a subprocess for `docker build`
    // because streaming the full build context through the bollard API requires
    // bundling the context into a tar archive first; the CLI handles that for us.
    let status = std::process::Command::new("docker")
        .args([
            "build",
            "-f",
            "docker/builder.Dockerfile",
            "-t",
            &builder_image,
            ".",
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("docker build failed with exit code: {status}");
    }

    // Container paths match prod (create_build_container in docker.rs).
    let container_pkgdest_dir = Path::new(CONTAINER_PKGDEST_DIR);
    let container_build_dir = Path::new(CONTAINER_BUILD_DIR);

    // Host build dir — tempdir keeps the test self-contained.
    let temp_dir = tempfile::tempdir()?;
    let host_build_dir = temp_dir.path().to_path_buf();
    std::fs::set_permissions(&host_build_dir, std::fs::Permissions::from_mode(0o777))?;

    // Build command — identical to what aurcache-builder sends to the container.
    let build_cmd = aurcache_builder::commands::build_build_command(
        &package,
        &build_flags,
        container_build_dir,
    );

    // makepkg.conf: no DB context → user settings skipped (same as prod's None path).
    let (makepkg_config, makepkg_config_path) =
        create_makepkg_config(None, container_pkgdest_dir).await?;

    // pacman.conf: standard repos, no [repo] section (no AURCache server running).
    let pacman_config = base_pacman_config(None);

    let cmd = aurcache_builder::commands::wrap_with_makepkg_config(
        &makepkg_config,
        &makepkg_config_path,
        &pacman_config,
        &build_cmd,
    );

    // Volume mounts — same logic as create_build_container:
    //   1. Build output dir
    //   2. Mirrorlist via the shared mirrorlist_mount() function, which correctly
    //      handles both bind-mount and named-volume-subpath cases.
    let arch = "linux/x86_64";
    let binds = vec![format!(
        "{}:{CONTAINER_PKGDEST_DIR}",
        host_build_dir.display()
    )];
    let mounts = mirrorlist_mount(arch)?
        .inspect(|m| {
            println!(
                "Mounting mirrorlist from {}",
                m.source.as_deref().unwrap_or("?")
            );
        })
        .into_iter()
        .collect::<Vec<_>>();
    if mounts.is_empty() {
        println!("No mirrorlist mount for {arch}; using image's baked mirrorlist");
    }

    // Container config — mirrors prod's create_build_container (no resource limits
    // or network config since there is no DB or AURCache server in the test).
    let conf = ContainerCreateBody {
        image: Some(builder_image.clone()),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        open_stdin: Some(false),
        user: Some("ab".to_string()),
        // Entrypoint matches prod's create_build_container.
        cmd: Some(vec![
            "bash".to_string(),
            "-leco".to_string(),
            "pipefail".to_string(),
            cmd,
        ]),
        host_config: Some(HostConfig {
            auto_remove: Some(false), // we remove it ourselves after inspecting output
            binds: Some(binds),
            mounts: Some(mounts),
            ..Default::default()
        }),
        ..Default::default()
    };

    let container = docker
        .create_container(
            Some(CreateContainerOptions {
                name: Some(format!("aurcache_test_{package}")),
                platform: arch.to_string(),
            }),
            conf,
        )
        .await?;

    // Attach before starting so we don't miss any early output.
    let mut attach = docker
        .attach_container(
            &container.id,
            Some(AttachContainerOptions {
                stdout: true,
                stderr: true,
                stdin: false,
                stream: true,
                ..Default::default()
            }),
        )
        .await?;

    docker
        .start_container(&container.id, None::<StartContainerOptions>)
        .await?;

    // Stream container output to stdout.
    while let Some(chunk) = attach.output.next().await {
        match chunk? {
            LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                print!("{}", String::from_utf8_lossy(&message));
            }
            _ => {}
        }
    }

    // Wait for the container to exit and check the status code.
    let mut wait_stream = docker.wait_container(&container.id, None::<WaitContainerOptions>);
    let exit_code = wait_stream
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("wait_container returned no result"))??
        .status_code;
    let _ = docker.remove_container(&container.id, None).await;
    if exit_code != 0 {
        anyhow::bail!("Container exited with status code {exit_code}");
    }

    println!();
    println!("=== Checking built package ===");

    let mut entries: Vec<_> = std::fs::read_dir(&host_build_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz")
        })
        .collect();

    if entries.is_empty() {
        anyhow::bail!("No package file found in {}", host_build_dir.display());
    }

    let pkgfile = entries.remove(0).path();
    println!("Found package: {}", pkgfile.display());

    let status = std::process::Command::new("tar")
        .args(["-tf", &pkgfile.to_string_lossy()])
        .stdout(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("Invalid archive file: {}", pkgfile.display());
    }

    println!("Archive is valid");
    println!("=== Builder test complete ===");

    Ok(())
}
