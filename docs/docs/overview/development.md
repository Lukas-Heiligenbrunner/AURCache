# Development
If you want to contribute to the project feel free to checkout the code and try to fix a bug or implement a feature.

## Build Info

The AURCache project comprises two main components, both Rust: a Dioxus frontend
compiled to WebAssembly and the backend.

### Frontend (Rust / Dioxus)

`frontend-rs/` is its own Cargo workspace, because it only builds for
`wasm32-unknown-unknown` and including it in the backend workspace would break
`cargo build --workspace`.

```bash
cd frontend-rs
cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test   # host target: the component logic, not the browser
```

There is no separate build step for it. `aurcache-api`'s build script compiles
the frontend to wasm and embeds it under the `static` feature, re-running
whenever the frontend changes:

```bash
cd backend && cargo run --features aurcache-api/static -p aurcache
```

### Backend (Rust)

To build the Rust backend, make sure you have Rust installed. Then, navigate to the backend directory and run:

```bash
cd backend
cargo build --release
```

#### `alpm-pkgbuild-bridge`

The backend parses PKGBUILDs by running [`alpm-pkgbuild-bridge`] through
`aurcache-sandbox`. The bridge is not bundled with a crate. Its path and the
sandbox's path must be absolute; set `AURCACHE_PKGBUILD_BRIDGE` and
`AURCACHE_SANDBOX` when they are installed outside their default locations.

```bash
# Arch
sudo pacman -S alpm-pkgbuild-bridge

# anything else
sudo curl -sSLf -o /usr/local/bin/alpm-pkgbuild-bridge \
  "https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge/-/raw/main/alpm-pkgbuild-bridge.sh?ref_type=heads"
sudo chmod 755 /usr/local/bin/alpm-pkgbuild-bridge
```

For a checkout, build the sandbox and point the tests at it from the repository
root:

```bash
cargo build --manifest-path backend/Cargo.toml -p aurcache-sandbox
export AURCACHE_SANDBOX="$PWD/backend/target/debug/aurcache-sandbox"
# If installed using the curl command above:
export AURCACHE_PKGBUILD_BRIDGE=/usr/local/bin/alpm-pkgbuild-bridge
```

An unpatched package can use its shipped `.SRCINFO`; applying a patch requires
a PKGBUILD re-parse through the bridge. Tests requiring the parser skip locally
when it is unavailable and fail under CI, so a passing local test run alone
does not prove parsing was exercised.

The Docker image installs it the same way (see `docker/server.Dockerfile`), so this
only affects running the backend or its tests directly on your machine.

[`alpm-pkgbuild-bridge`]: https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge

## Tests and lints

These are what CI runs, so it is worth running them before opening a PR:

```bash
# backend — needs alpm-pkgbuild-bridge (see above)
cd backend
cargo fmt -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# frontend
cd frontend-rs
cargo fmt -- --check
cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test
```

The root `Justfile` provides `just format`, `just lint`, and `just test` for both
workspaces. `just lint` treats warnings as errors and checks the frontend for
both host and wasm targets. Use `just test-browser` to verify that the embedded
UI mounts and its interactions work in a real browser; host tests exercise
the component logic without mounting the application.

### API types are shared, not mirrored

There is no code generation step. The types the API speaks live in
`aurcache-common` and are used directly by the server, the CLI and the
frontend, which reaches them through the same `aurcache-client` crate the CLI
uses. A field added to a response is added once and all three see it — mirroring
a struct by hand is how the two ends drift apart.

### Api Docs
You can access the API docs (scalar) `http://localhost:8080/docs` after starting the backend.

Or if you prefer Redoc `http://localhost:8080/redoc`.
