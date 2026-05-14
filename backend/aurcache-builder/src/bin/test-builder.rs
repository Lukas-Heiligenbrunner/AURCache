use aurcache_db::packages::SourceData;
use std::path::Path;
use std::process::Command;

fn main() -> anyhow::Result<()> {
    let package = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "hello".to_string());
    let builder_image = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "aurcache-builder:test".to_string());
    let build_flags = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "-B --noconfirm --noprogressbar --color never --pgpfetch".to_string());

    println!("=== Testing builder image: {builder_image} ===");
    println!("Building package: {package}");
    println!("Build flags: {build_flags}");

    // Build the Docker image
    let status = Command::new("docker")
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
        anyhow::bail!("docker build failed with exit code: {}", status);
    }

    // Create a temporary build directory
    let temp_dir = tempfile::tempdir()?;
    let build_dir = temp_dir.path().join("test_builds");
    std::fs::create_dir_all(&build_dir)?;

    let container_build_dir = Path::new("/build").join("src");

    // Construct the exact same command that aurcache-builder would use
    let source_data = SourceData::Aur {
        name: package.clone(),
    };

    let build_cmd = aurcache_builder::commands::build_build_command(
        &source_data,
        &package,
        &build_flags,
        &container_build_dir,
    );

    let (makepkg_config, makepkg_config_path) =
        aurcache_builder::makepkg_utils::create_makepkg_config_minimal(Path::new("/build"));

    let cmd = aurcache_builder::commands::wrap_with_makepkg_config(
        &makepkg_config,
        &makepkg_config_path,
        "[options]\nDisableSandbox\nSigLevel = Never\n",
        &build_cmd,
    );

    // Run the build container
    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/build", build_dir.display()),
            "--user",
            "ab",
            &builder_image,
            "sh",
            "-lec",
            &cmd,
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("docker run failed with exit code: {}", status);
    }

    println!();
    println!("=== Checking built package ===");

    let mut entries: Vec<_> = std::fs::read_dir(&build_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz")
        })
        .collect();

    if entries.is_empty() {
        anyhow::bail!("No package file found in {}", build_dir.display());
    }

    let pkgfile = entries.remove(0).path();
    println!("Found package: {}", pkgfile.display());

    let status = Command::new("tar")
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
