use std::{fs, path::Path, process::Command};
use std::io;

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), dest_path)?;
        }
    }
    Ok(())
}

fn main() {
    let frontend_dir = Path::new("../../frontend");

    println!("cargo:rerun-if-changed=../../frontend/lib/");
    println!("cargo:rerun-if-changed=../../frontend/pubspec.yaml");

    // 1️⃣ Ensure Flutter dependencies are installed
    let status = Command::new("flutter")
        .args(["pub", "get"])
        .current_dir(frontend_dir)
        .status()
        .expect("Failed to run flutter pub get");
    if !status.success() {
        panic!("flutter pub get failed!");
    }

    // 2️⃣ Run build_runner to generate source files
    let status = Command::new("flutter")
        .args(["pub", "run", "build_runner", "build", "--delete-conflicting-outputs"])
        .current_dir(frontend_dir)
        .status()
        .expect("Failed to run flutter pub run build_runner build");
    if !status.success() {
        panic!("flutter build_runner build failed!");
    }

    // 3️⃣ Build Flutter web frontend
    let status = Command::new("flutter")
        .args(["build", "web", "--release"])
        .current_dir(frontend_dir)
        .status()
        .expect("Failed to run flutter build web");
    if !status.success() {
        panic!("flutter build web failed!");
    }

    // 4️⃣ Copy built web files into OUT_DIR
    let out_dir = ".".to_string();
    let dest = Path::new(&out_dir).join("frontend_build");

    if dest.exists() {
        fs::remove_dir_all(&dest).unwrap();
    }

    let build_dir = frontend_dir.join("build/web");
    copy_dir_all(&build_dir, &dest).expect("Failed to copy built frontend");

    println!("cargo:rerun-if-changed=frontend/build/web");
}
