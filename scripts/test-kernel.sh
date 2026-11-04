#!/usr/bin/env bash
# Tests that need a real kernel and root: the storage pool (loop devices,
# btrfs, simple quotas) and the build cgroups.
#
# They are opt-in under `cargo test` -- the pool tests behind
# AURCACHE_POOL_TESTS, the cgroup tests `#[ignore]`d -- because they mount
# filesystems and move the caller's cgroup. Here they run as root in a
# throwaway privileged container, so neither touches the host: the test
# binaries are built on the host, and the container supplies root, btrfs-progs
# and a private cgroup namespace.
#
# The pool needs Linux 6.7 or later (btrfs simple quotas). On an older kernel
# those tests are skipped with a warning and the cgroup tests still run.
#
# Usage: scripts/test-kernel.sh
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib/common.sh
. scripts/lib/common.sh
ROOT=$PWD

say() { printf '==> %s\n' "$*"; }
warn() {
    # A GitHub annotation under Actions, a plain line elsewhere.
    if [[ -n ${GITHUB_ACTIONS:-} ]]; then echo "::warning::$*"; else echo "warning: $*" >&2; fi
}

# Each target: package, test, the arguments it runs with.
TARGETS=(
    "aurcache-chroot pool"
    "aurcache-worker pool_drain"
    "aurcache-worker cache_volumes"
    "aurcache-worker cgroup_peak --ignored"
)

kernel=$(uname -r)
IFS=. read -r major minor _ <<<"$kernel"
pool=1
if (( major < 6 || (major == 6 && minor < 7) )); then
    warn "kernel $kernel has no btrfs simple quotas (6.7+): skipping the pool tests"
    pool=0
fi

say "building the test binaries"
# Cargo names each test binary, which a path guessed from the target
# directory's hash would not do reliably.
binaries=()
for target in "${TARGETS[@]}"; do
    read -r package test _ <<<"$target"
    exe=$(cargo_executable "$test" test --manifest-path backend/Cargo.toml \
        -p "$package" --test "$test" --no-run)
    binaries+=("$exe")
done

# The binaries carry absolute paths (CARGO_TARGET_TMPDIR, where the pool tests
# put their images), so the tree and the target directory -- which
# CARGO_TARGET_DIR may put elsewhere -- are mounted at the same paths inside.
# The target directory is where root writes, so it has to be local storage:
# on NFS, root is usually squashed and every test fails with EACCES.
TARGET=$(cargo metadata --manifest-path backend/Cargo.toml --format-version 1 --no-deps \
    | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
script='set -euo pipefail
pacman -Sy --noconfirm --needed btrfs-progs sudo util-linux e2fsprogs >/dev/null
# The tests make and mount their own images; `sudo -n` as root needs no grant.
export AURCACHE_POOL_TESTS=1
status=0
while (( $# )); do
    exe=$1 args=$2; shift 2
    echo "==> ${exe##*/} $args"
    # shellcheck disable=SC2086
    "$exe" $args --test-threads=1 || status=1
done
exit $status'

run=()
for i in "${!TARGETS[@]}"; do
    read -r _ test args <<<"${TARGETS[$i]}"
    if [[ $test != cgroup_peak ]] && (( ! pool )); then continue; fi
    run+=("${binaries[$i]}" "${args:-}")
done

say "running as root in a privileged container"
docker run --rm --privileged --cgroupns=private \
    -v "$ROOT:$ROOT" -v "$TARGET:$TARGET" -w "$ROOT" \
    archlinux/archlinux:latest bash -c "$script" _ "${run[@]}"
say "ok"
