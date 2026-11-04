# shellcheck shell=bash
# Helpers the scripts under scripts/ share. Sourced, never run; the caller sets
# REPO_ROOT to the checkout.

# A script's help text is its header comment: from the second line down to
# `set -euo`, with the comment markers stripped.
header_help() {
    sed -n '2,/^set -euo/p' "$1" | sed 's/^# \?//; $d'
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
