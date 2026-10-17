//! Builds the web frontend and puts it where `RustEmbed` will find it.
//!
//! `#[derive(RustEmbed)] #[folder = "web"]` bakes the bundle in at compile
//! time, so a stale `web/` means a server quietly serving an old UI. Doing it
//! from here rather than from a script means the dependency is expressed rather
//! than remembered: the `rerun-if-changed` lines below make cargo rebuild the
//! server whenever the frontend changes, and nobody has to notice.
//!
//! **This does nothing unless the `static` feature is on.** A build script
//! cannot be feature-gated away — it always runs — so it checks the feature
//! itself and returns. Building the server without `static` therefore costs
//! nothing here and needs no wasm toolchain, which is what someone working only
//! on the backend gets.
//!
//! The check reads `CARGO_FEATURE_STATIC` rather than `cfg!(feature = "static")`.
//! Both work today — the `cfg!` was measured, not assumed — but only the
//! environment variable is specified: the Cargo reference lists
//! `CARGO_FEATURE_<name>` as how features reach a build script and says nothing
//! about `cfg!`. The same page warns that `cfg!` in a build script describes the
//! *host* it runs on rather than the target being built, which is why
//! `target_os` and friends must be read from `CARGO_CFG_*`. Features happen not
//! to trip over that; target cfgs do, and this project cross-compiles for
//! arm64. Reading them all the same way keeps that trap shut.
//!
//! The nested cargo invocation is safe because `frontend-rs` is its own
//! workspace with its own target directory: the two builds never contend for
//! the same lock. The cargo-set environment is cleared for the child all the
//! same, since inheriting `CARGO_TARGET_DIR` would undo exactly that.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Cheap and unconditional: without it, adding the feature later would not
    // rerun this script and the first `static` build would embed nothing.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_STATIC");

    if std::env::var_os("CARGO_FEATURE_STATIC").is_none() {
        return;
    }

    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR"),
    );
    let frontend = manifest
        .join("../../frontend-rs")
        .canonicalize()
        .expect("frontend-rs sits beside the backend workspace");
    let web = manifest.join("web");

    // What makes a rebuild automatic. Cargo walks a directory given here, so
    // the sources are covered by naming `src` rather than every file in it.
    for path in ["src", "Cargo.toml", "Cargo.lock", "index.html"] {
        println!("cargo:rerun-if-changed={}", frontend.join(path).display());
    }
    let logo = manifest.join("../../assets/icons/icon.svg");
    println!("cargo:rerun-if-changed={}", logo.display());

    build_wasm(&frontend);
    bindgen(&frontend);
    install(&frontend.join("dist"), &web);
    // The project logo, shared with the README and the docs rather than
    // copied into the frontend: the sidebar shows it and the page uses it as
    // its favicon, both as `/icon.svg`.
    std::fs::copy(&logo, web.join("icon.svg")).expect("copy the logo");
}

/// Compile the frontend crate to wasm.
fn build_wasm(frontend: &Path) {
    run(
        Command::new(cargo()).current_dir(frontend).args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
        ]),
        "cargo build (frontend)",
        "Is the wasm32-unknown-unknown target installed? \
         `rustup target add wasm32-unknown-unknown`",
    );
}

/// Turn the wasm into something a browser can load: the JS glue the page
/// imports, beside a processed copy of the wasm, in `dist/`.
///
/// Done by the `wasm-bindgen-cli-support` library, a build dependency, rather
/// than by the `wasm-bindgen` command. The two halves of wasm-bindgen -- the
/// crate the frontend links and what generates its glue -- must be exactly the
/// same version, and refuse any other pair. An installed command matched or
/// did not depending on what the machine had (Arch's package, for one, follows
/// its own schedule), and the frontend was held back to whatever that was. As
/// a build dependency pinned to the frontend's version, it is fetched and
/// built like any other crate, in every build, offline ones included.
#[cfg(feature = "static")]
fn bindgen(frontend: &Path) {
    check_pair(frontend);
    wasm_bindgen_cli_support::Bindgen::new()
        .input_path(frontend.join("target/wasm32-unknown-unknown/release/aurcache-frontend.wasm"))
        .web(true)
        .and_then(|bindgen| {
            bindgen
                .typescript(false)
                // The command's default, not the library's: without it the
                // glue's `init()` has no idea where its `.wasm` is, and the
                // page calls it with no arguments.
                .omit_default_module_path(false)
                .generate(frontend.join("dist"))
        })
        .unwrap_or_else(|e| panic!("wasm-bindgen failed: {e:#}"));
}

#[cfg(not(feature = "static"))]
fn bindgen(_frontend: &Path) {
    unreachable!("only run under the `static` feature, which brings the bindgen library");
}

/// Refuse a frontend locked to another wasm-bindgen than the library here.
///
/// Both manifests pin the same exact version, so this only fires after one of
/// the two pins was changed alone. The library would refuse too, but in terms
/// of "this binary" and a schema version, with no word of which file to edit.
#[cfg(feature = "static")]
fn check_pair(frontend: &Path) {
    let locked = |lockfile: &Path, name: &str| -> Option<String> {
        let text = std::fs::read_to_string(lockfile).ok()?;
        let entry = format!("name = \"{name}\"\nversion = \"");
        let at = text.find(&entry)? + entry.len();
        Some(text[at..].split('"').next()?.to_string())
    };
    let backend_lock = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR"),
    )
    .join("../Cargo.lock");
    let crate_version = locked(&frontend.join("Cargo.lock"), "wasm-bindgen");
    let library_version = locked(&backend_lock, "wasm-bindgen-cli-support");
    if crate_version != library_version {
        panic!(
            "frontend-rs is locked to wasm-bindgen {} but the bindgen library here is {}; \
             the two only work as a pair. Pin both to the same version: `wasm-bindgen` in \
             frontend-rs/Cargo.toml and `wasm-bindgen-cli-support` in \
             backend/aurcache-api/Cargo.toml.",
            crate_version.as_deref().unwrap_or("<unknown>"),
            library_version.as_deref().unwrap_or("<unknown>"),
        );
    }
}

/// Copy the bundle, and the page that loads it, into the embed folder.
///
/// The folder is emptied first: a file dropped from the frontend would
/// otherwise stay embedded indefinitely.
///
/// `index.html` is written *after* the bundle, and the order is load-bearing.
/// `dist/` is wasm-bindgen's output directory, but nothing guarantees it holds
/// only wasm-bindgen's output — a `dist/index.html` left by an earlier tool sat
/// there for months, and copying the tree last meant that stale copy quietly
/// replaced the real one on every build. The symptom was an edit to the page's
/// CSS that simply never appeared, with a correctly rebuilt wasm alongside it
/// to make the bundle look fresh.
fn install(dist: &Path, web: &Path) {
    let _ = std::fs::remove_dir_all(web);
    std::fs::create_dir_all(web).expect("create web/");

    copy_tree(dist, web);
    std::fs::copy(
        dist.parent().expect("dist has a parent").join("index.html"),
        web.join("index.html"),
    )
    .expect("copy index.html");
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read dist/") {
        let entry = entry.expect("read dist/ entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            std::fs::create_dir_all(&target).expect("create dir");
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// The cargo that is running this build, so the child uses the same toolchain.
fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// Run a command, failing the build with something legible if it does not work.
fn run(command: &mut Command, what: &str, hint: &str) {
    // Cargo exports its own build's settings, and a child cargo would take
    // them as its own — `CARGO_TARGET_DIR` most of all, which would point the
    // frontend at the server's target directory and deadlock on its lock.
    for key in [
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "RUSTC",
        "RUSTDOC",
        "CARGO_MAKEFLAGS",
    ] {
        command.env_remove(key);
    }

    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("{what} failed with {status}. {hint}"),
        Err(e) => panic!("could not run {what}: {e}. {hint}"),
    }
}
