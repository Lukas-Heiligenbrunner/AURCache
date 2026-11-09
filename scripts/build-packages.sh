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
# src/ pkg/ and a tarball inside `packaging/` for git to notice. See
# packaging/build-from-tree.sh, which the images' packager stage runs too.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname -- "$SCRIPT_DIR")"
readonly SCRIPT_DIR REPO_ROOT

# `aurcache-worker` depends on `aurcache-sandbox`, so the default builds both:
# installing the worker alone would ask pacman for a sandbox it cannot find.
readonly DEFAULT_PACKAGES="aurcache-sandbox aurcache-worker"
readonly ALL_PACKAGES="aurcache-sandbox aurcache-worker aurcache-server aurcache-worker-docker aurcache-cli"

# shellcheck source=scripts/lib/common.sh
. "$SCRIPT_DIR/lib/common.sh"

usage() {
    header_help "${BASH_SOURCE[0]}"
    cat <<'USAGE'
Options:
  -p, --packages LIST   Space- or comma-separated package names, or `all`
                        (default: aurcache-sandbox aurcache-worker)
  -o, --out DIR         Where to build (default: a temporary directory)
      --install         `sudo pacman -U` everything built, in dependency order
  -h, --help            This message
USAGE
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
    echo
    echo "==> $pkg"
    archives=$("$REPO_ROOT/packaging/build-from-tree.sh" "$REPO_ROOT" "$pkg" "$outdir")
    mapfile -t -O "${#built[@]}" built <<<"$archives"
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
