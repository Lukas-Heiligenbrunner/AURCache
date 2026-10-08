#!/usr/bin/env bash
# Build the Arch packages from this working tree, and optionally install them.
#
# The same thing the container images do in their packager stage, runnable on a
# host. Each package is built from the *current tree* rather than from a release
# tarball -- `make-source-tarball.sh` writes the archive the PKGBUILD expects
# under the name it expects, so one PKGBUILD serves both.
#
#   scripts/build-packages.sh                    # worker + its sandbox
#   scripts/build-packages.sh --install          # ... and pacman -U them
#   scripts/build-packages.sh --packages aurcache-server
#   scripts/build-packages.sh --packages all
#
# Built out of tree, under a directory of its own, so `makepkg` never leaves
# src/ pkg/ and a tarball inside `packaging/` for git to notice.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname -- "$SCRIPT_DIR")"
readonly SCRIPT_DIR REPO_ROOT

# `aurcache-worker` depends on `aurcache-sandbox`, so the default builds both:
# installing the worker alone would ask pacman for a sandbox it cannot find.
readonly DEFAULT_PACKAGES="aurcache-sandbox aurcache-worker"
readonly ALL_PACKAGES="aurcache-sandbox aurcache-worker aurcache-server aurcache-worker-docker aurcache-cli"

usage() {
    sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \?//; $d'
    cat <<'USAGE'
Options:
  -p, --packages LIST   Space- or comma-separated package names, or `all`
                        (default: aurcache-sandbox aurcache-worker)
  -o, --out DIR         Where to build (default: a temporary directory)
      --install         `sudo pacman -U` everything built, in dependency order
  -h, --help            This message
USAGE
}

# The PKGBUILDs are submodules of the AUR repositories they are published from,
# so a fresh clone has empty directories until they are checked out. Without
# this the failure is a confusing one deep inside makepkg or the image build,
# about a PKGBUILD that is simply not there.
require_packaging_submodules() {
    local missing=()
    local pkg
    for pkg in "$@"; do
        [[ -f "$REPO_ROOT/packaging/$pkg/PKGBUILD" ]] || missing+=("$pkg")
    done
    if ((${#missing[@]})); then
        echo "error: no PKGBUILD for: ${missing[*]}" >&2
        echo "       packaging/ holds git submodules of the AUR repositories." >&2
        echo "       Check them out with:  git submodule update --init" >&2
        exit 2
    fi
}

packages=$DEFAULT_PACKAGES
outdir=
install=0

while (($#)); do
    case $1 in
        -p | --packages) packages=${2//,/ }; shift 2 ;;
        -o | --out) outdir=$2; shift 2 ;;
        --install) install=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[[ $packages == all ]] && packages=$ALL_PACKAGES
require_packaging_submodules $packages

if [[ -z $outdir ]]; then
    outdir=$(mktemp -d -t aurcache-packages-XXXXXX)
    echo "==> building in $outdir"
fi
mkdir -p "$outdir"

built=()
for pkg in $packages; do
    src="$REPO_ROOT/packaging/$pkg"
    [[ -d $src ]] || { echo "error: no such package: $pkg" >&2; exit 2; }

    # Each AUR submodule owns its version and the source archive name it needs.
    PKGVER="$(awk -F= '/^pkgver=/ {print $2; exit}' "$src/PKGBUILD")"

    # A copy per package: makepkg writes src/, pkg/ and the tarball beside the
    # PKGBUILD, and doing that inside the repository dirties the tree.
    work="$outdir/$pkg"
    rm -rf "$work"
    cp -r "$src" "$work"

    echo
    echo "==> $pkg $PKGVER"
    (
        cd "$work"
        "$REPO_ROOT/packaging/make-source-tarball.sh" "$REPO_ROOT" "$pkg" "$PKGVER" .
        # --skipinteg because the tarball is this tree rather than a release, so
        # it cannot match a release checksum. --nodeps because the dependencies
        # are recorded in the package and resolved where it is installed, which
        # is not necessarily here.
        makepkg --skipinteg --noconfirm --nocheck --nodeps
    )
    # `ls` rather than a glob so a stale package from an earlier version cannot
    # be picked up silently.
    built+=("$(ls -t "$work"/*.pkg.tar.zst | head -1)")
done

echo
printf '==> built:\n'
printf '    %s\n' "${built[@]}"

if ((install)); then
    echo
    echo "==> installing"
    # One transaction, so pacman resolves the inter-package dependencies itself
    # rather than refusing the worker for want of a sandbox it is about to get.
    sudo pacman -U --noconfirm "${built[@]}"
fi
