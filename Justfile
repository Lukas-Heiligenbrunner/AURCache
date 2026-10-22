# The `static` feature makes aurcache-api's build script compile the frontend
# to wasm and embed it, re-running whenever the frontend changes. There is
# nothing to build first and nothing to keep in step.
#
# Run a server with the UI inside it, as the container ships it
serve:
  cd backend && cargo run --features aurcache-api/static -p aurcache

# Remove build output, including the embedded frontend
clean:
  cd backend && cargo clean
  cd frontend-rs && cargo clean
  rm -rf backend/aurcache-api/web frontend-rs/dist

# Format both workspaces
format:
  cd backend && cargo fmt --all
  cd frontend-rs && cargo fmt

# Lint both workspaces, and the frontend for both its targets
lint:
  cd backend && cargo clippy --all-targets -- -D warnings
  # The crate ships as wasm; its browser tests are host-only. Both are linted.
  cd frontend-rs && cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings
  cd frontend-rs && cargo clippy --all-targets -- -D warnings

# Unit and integration tests, no browser
test:
  cd backend && cargo test --all
  cd frontend-rs && cargo test

# Render every route in a real browser and drive the interactive ones
test-browser:
  ./scripts/test-frontend.sh

# The storage pool and cgroup tests, as root in a privileged container
test-kernel:
  ./scripts/test-kernel.sh
