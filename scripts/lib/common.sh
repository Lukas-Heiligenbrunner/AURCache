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

# Build with `cargo "$@"` and print the path of the executable target named
# `$1`. Cargo says where it put it: a path assumed under backend/target would
# miss a CARGO_TARGET_DIR, or find a stale binary there instead.
cargo_executable() {
    local name=$1
    shift
    local exe
    exe=$(cargo "$@" --message-format=json | python3 -c '
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("reason") == "compiler-artifact" and msg.get("executable") \
            and msg["target"]["name"] == sys.argv[1]:
        print(msg["executable"])' "$name")
    [[ -n $exe ]] || { echo "error: cargo built no executable named $name" >&2; return 1; }
    printf '%s\n' "$exe"
}
