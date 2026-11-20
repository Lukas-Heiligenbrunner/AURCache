#!/usr/bin/env bash
# Build the prebuilt `aurcli` archives attached to a GitHub release.
#
# What the release-binaries workflow runs, and runnable by hand. Each target
# becomes `aurcli-<target>.tar.gz` -- the `aurcli` binary at the top of the
# archive, beside the repository's LICENSE and the CLI's own README.md -- and
# `aurcli-<target>.tar.gz.sha256`, in sha256sum's format. Those names and that
# layout are a published contract: install instructions and scripts download
# them by name. The cargo-binstall metadata in backend/aurcache-cli/Cargo.toml
# names the same files, so the two change together.
#
#   scripts/build-cli-binaries.sh
#   scripts/build-cli-binaries.sh --target aarch64-unknown-linux-musl --out-dir /srv/dist
#
# Needs rustup (it installs the stable toolchain's targets), tar, and
# sha256sum or shasum. The Linux targets also need cargo-zigbuild and zig on
# PATH; the macOS targets need Apple's command line tools.
#
# The Linux targets are musl and fully static, so one binary runs on any
# distribution. They cross-compile from any Linux host through cargo-zigbuild,
# with zig as the C compiler and linker for the C in aws-lc-sys.
#
# armv7 needs a zig whose lib directory is writable, because cargo-zigbuild
# adds a header to it: a distribution's /usr/lib/zig fails with "Permission
# denied", where a release tarball from ziglang.org unpacked anywhere works.
#
# The macOS targets build with Apple's toolchain, so only on macOS; either one
# builds on either Mac architecture.
#
# Without --target, every target this host can build is built.
#
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname -- "$SCRIPT_DIR")"
readonly SCRIPT_DIR REPO_ROOT

readonly LINUX_TARGETS=(
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
    armv7-unknown-linux-musleabihf
)
readonly MACOS_TARGETS=(
    x86_64-apple-darwin
    aarch64-apple-darwin
)

# shellcheck source=scripts/lib/common.sh
. "$SCRIPT_DIR/lib/common.sh"

usage() {
    header_help "${BASH_SOURCE[0]}"
    cat <<'EOF'
Options:
  -t, --target TRIPLE   Rust target to build; repeat for several. One of:
                        x86_64-unknown-linux-musl, aarch64-unknown-linux-musl,
                        armv7-unknown-linux-musleabihf, x86_64-apple-darwin,
                        aarch64-apple-darwin
  -o, --out-dir DIR     Where the archives and checksums go
                        (default: cli-binaries/ under Cargo's target directory)
  -h, --help            This message
EOF
}

targets=()
out_dir=

# An option that takes a value, given as the last argument.
need_value() {
    if (($# < 2)); then
        echo "error: $1 needs a value" >&2
        usage >&2
        exit 2
    fi
}

while (($#)); do
    case $1 in
        -t | --target) need_value "$@"; targets+=("$2"); shift 2 ;;
        -o | --out-dir) need_value "$@"; out_dir=$2; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

# Relative to where the script was run from, not to the tree it builds.
if [[ -n $out_dir ]]; then
    mkdir -p "$out_dir"
    out_dir=$(cd -- "$out_dir" && pwd)
fi

host_os=$(uname -s)
if ((${#targets[@]} == 0)); then
    case $host_os in
        Linux) targets=("${LINUX_TARGETS[@]}") ;;
        Darwin) targets=("${MACOS_TARGETS[@]}") ;;
        *) echo "error: no targets build on $host_os; pass --target" >&2; exit 2 ;;
    esac
fi

for target in "${targets[@]}"; do
    case $target in
        *-unknown-linux-musl*)
            [[ " ${LINUX_TARGETS[*]} " == *" $target "* ]] || {
                echo "error: unknown target '$target'" >&2
                exit 2
            }
            if ! command -v cargo-zigbuild >/dev/null || ! command -v zig >/dev/null; then
                echo "error: $target builds with cargo-zigbuild, which needs both" >&2
                echo "       cargo-zigbuild and zig on PATH" >&2
                exit 2
            fi
            ;;
        *-apple-darwin)
            [[ " ${MACOS_TARGETS[*]} " == *" $target "* ]] || {
                echo "error: unknown target '$target'" >&2
                exit 2
            }
            if [[ $host_os != Darwin ]]; then
                echo "error: $target builds only on macOS" >&2
                exit 2
            fi
            ;;
        *) echo "error: unknown target '$target'" >&2; exit 2 ;;
    esac
done

# The release toolchain, as the PKGBUILDs build with (packaging/common.sh),
# whatever this checkout's default is.
export RUSTUP_TOOLCHAIN=stable
# Strip the symbols too, through the release profile: the workspace's own
# release profile only strips debuginfo.
export CARGO_PROFILE_RELEASE_STRIP=symbols
# Compiler flags chosen for this machine -- makepkg's `-march=x86-64` and
# `-flto=auto`, say -- reach the C in aws-lc-sys and break a cross build, and
# a release binary should not depend on who built it anyway.
unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS
# The same for rustc: an empty CARGO_ENCODED_RUSTFLAGS takes precedence over
# RUSTFLAGS and over any rustflags in the caller's Cargo configuration.
export CARGO_ENCODED_RUSTFLAGS=

cd "$REPO_ROOT/backend"

cargo --version
rustc --version

metadata=$(cargo metadata --format-version 1 --no-deps)
# A path with a quote or a backslash in it would be escaped in the JSON; such
# a target directory is refused rather than misread.
target_dir_re='"target_directory":"([^"\]*)"'
if [[ ! $metadata =~ $target_dir_re ]]; then
    echo "error: cannot read Cargo's target directory from cargo metadata" >&2
    exit 1
fi
target_dir=${BASH_REMATCH[1]}
if [[ -z $out_dir ]]; then
    out_dir=$target_dir/cli-binaries
    mkdir -p "$out_dir"
fi

# sha256sum's format, from whichever tool this system has: macOS ships
# shasum, which writes the same lines.
checksum() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$@"
    else
        shasum -a 256 "$@"
    fi
}

# The archive's entries belong to root rather than to whoever built them, so
# an extraction as root does not hand the files to some unrelated local uid.
# GNU tar (Linux) and bsdtar (macOS) spell that differently. bsdtar would also
# record the files' extended attributes and ACLs -- macOS's provenance and
# quarantine attributes, say -- as pax headers, which nothing extracting the
# archive wants.
if [[ $(tar --version 2>/dev/null) == *"GNU tar"* ]]; then
    tar_flags=(--owner=0 --group=0 --numeric-owner)
else
    tar_flags=(--uid 0 --gid 0 --numeric-owner --no-xattrs --no-mac-metadata --no-acls)
fi

for target in "${targets[@]}"; do
    echo "==> building aurcli for $target"
    rustup target add "$target"
    case $target in
        *-unknown-linux-musl*) build=zigbuild ;;
        *) build=build ;;
    esac
    cargo "$build" --locked --release -p aurcache-cli --target "$target"

    name=aurcli-$target
    # The archive lists its three files by name, so anything else left in
    # the staging directory stays out of it.
    stage=$target_dir/$target/cli-archive/$name
    mkdir -p "$stage"
    cp "$target_dir/$target/release/aurcli" "$stage/aurcli"
    cp "$REPO_ROOT/LICENSE" "$stage/"
    cp "$REPO_ROOT/backend/aurcache-cli/README.md" "$stage/README.md"

    # COPYFILE_DISABLE keeps macOS's tar from adding AppleDouble `._` entries.
    COPYFILE_DISABLE=1 tar "${tar_flags[@]}" -C "$stage" -czf "$out_dir/$name.tar.gz" \
        aurcli LICENSE README.md
    (cd "$out_dir" && checksum "$name.tar.gz" >"$name.tar.gz.sha256")
    echo "==> $out_dir/$name.tar.gz"
done
