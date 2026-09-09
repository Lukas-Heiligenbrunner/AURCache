#!/usr/bin/env bash
# Prove that parsing a PKGBUILD on the server cannot reach the server's data.
#
# `alpm-srcinfo` parses a PKGBUILD by shelling out to `alpm-pkgbuild-bridge`,
# which parses it by *sourcing* it: adding a package runs its PKGBUILD in the
# server's own process tree. docker/bridge-wrapper.sh confines that, and this
# checks the confinement rather than assuming it.
#
# Each payload is run twice: once through the wrapper, where it must fail, and
# once against the bare bridge, where it must succeed. A payload that fails for
# some unrelated reason -- a typo, a path that does not exist -- would otherwise
# report the sandbox working when it is doing nothing.
#
# Usage: scripts/test-sandbox.sh
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

sandbox=$root/backend/target/release/aurcache-sandbox
bridge=${AURCACHE_PKGBUILD_BRIDGE:-$work/alpm-pkgbuild-bridge}
bridge_url=https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge/-/raw/main/alpm-pkgbuild-bridge.sh?ref_type=heads

echo "==> building aurcache-sandbox"
(cd "$root/backend" && cargo build --release -p aurcache-sandbox)

if [[ ! -x $bridge ]]; then
    echo "==> fetching alpm-pkgbuild-bridge"
    curl -sSLf -o "$bridge" "$bridge_url"
    chmod +x "$bridge"
fi

# Stands in for /app: the database, the repository and the CA live there.
protected=$work/app
mkdir -p "$protected/repo"
echo SECRET-DB-CONTENT > "$protected/db.sqlite"
outside=$work/elsewhere
mkdir -p "$outside"

# What a parse legitimately gets: a temp directory holding one PKGBUILD, which
# is how the server calls it (`parse_pkgbuild` writes to a tempdir).
parse=$work/parse
mkdir -p "$parse"
cat > "$parse/PKGBUILD" <<EOF
pkgname=hostile-fixture
pkgver=1
pkgrel=1
arch=('x86_64')
license=('MIT')
source=()

# Read the server's data. The redirect lands in the parse directory, which is
# writable either way, so a non-empty file isolates a successful read.
\$(cat "$protected/db.sqlite" > ./read-protected 2>/dev/null || true)

# Rewrite the server's data, and write anywhere outside the parse directory.
\$(echo pwned > "$protected/repo/EVIL" 2>/dev/null || true)
\$(echo pwned > "$outside/EVIL" 2>/dev/null || true)

# Read a secret straight out of the environment, which Landlock cannot stop --
# the wrapper's env -i is what closes this one.
\$(printf '%s' "\${DB_PWD:-}" > ./env-leak 2>/dev/null || true)

# Must still work: the parse's own directory stays writable, or the wrapper
# would break parsing rather than confining it.
\$(touch ./legit-write 2>/dev/null || true)
EOF

reset_evidence() {
    rm -f "$parse/read-protected" "$parse/env-leak" "$parse/legit-write" \
          "$protected/repo/EVIL" "$outside/EVIL"
}

# Returns the four outcomes as a line of yes/no, so both runs are read the same
# way and the control cannot silently differ from the real one.
observe() {
    local read_ok=no write_ok=no outside_ok=no env_ok=no legit=no
    [[ -s $parse/read-protected ]] && read_ok=yes
    [[ -e $protected/repo/EVIL ]] && write_ok=yes
    [[ -e $outside/EVIL ]] && outside_ok=yes
    [[ -s $parse/env-leak ]] && env_ok=yes
    [[ -e $parse/legit-write ]] && legit=yes
    echo "read=$read_ok write=$write_ok outside=$outside_ok env=$env_ok legit=$legit"
}

echo "==> control: the bare bridge, to show the payloads work at all"
reset_evidence
# `cd` into the parse directory the way the wrapper does, so the only
# difference between the two runs is the confinement.
(cd "$parse" && DB_PWD=hunter2 "$bridge" ./PKGBUILD) > "$work/control.out" 2>"$work/control.err" || true
control=$(observe)
echo "    $control"
[[ $control == "read=yes write=yes outside=yes env=yes legit=yes" ]] || {
    echo "FAIL: the unconfined bridge did not run the payloads; the fixture is broken" >&2
    sed -n '1,20p' "$work/control.err" >&2
    exit 1
}

echo "==> confined: through docker/bridge-wrapper.sh"
reset_evidence
set +e
DB_PWD=hunter2 \
AURCACHE_SANDBOX=$sandbox \
AURCACHE_PKGBUILD_BRIDGE=$bridge \
AURCACHE_PROTECTED_DIR=$protected \
    "$root/docker/bridge-wrapper.sh" "$parse/PKGBUILD" > "$work/confined.out" 2>"$work/confined.err"
status=$?
set -e
if grep -q 'refusing to run unconfined' "$work/confined.err"; then
    echo "SKIP: this kernel cannot enforce Landlock:" >&2
    cat "$work/confined.err" >&2
    exit 77
fi
confined=$(observe)
echo "    $confined"

fail=0
[[ $confined == *"read=no"* ]]    || { echo "FAIL: the PKGBUILD read the protected directory" >&2; fail=1; }
[[ $confined == *"write=no"* ]]   || { echo "FAIL: the PKGBUILD wrote into the protected directory" >&2; fail=1; }
[[ $confined == *"outside=no"* ]] || { echo "FAIL: the PKGBUILD wrote outside its parse directory" >&2; fail=1; }
[[ $confined == *"env=no"* ]]     || { echo "FAIL: the PKGBUILD read a secret from the environment" >&2; fail=1; }
[[ $confined == *"legit=yes"* ]]  || { echo "FAIL: the parse directory was not writable" >&2; fail=1; }

# The point is to confine parsing, not to break it: the confined run must still
# produce the same parse the control did.
if ! grep -q 'hostile-fixture' "$work/confined.out"; then
    echo "FAIL: the confined parse produced no output" >&2
    sed -n '1,20p' "$work/confined.err" >&2
    fail=1
fi
[[ $status -eq 0 ]] || { echo "FAIL: the confined parse exited $status" >&2; fail=1; }

if (( fail )); then
    exit 1
fi
echo "==> PASS: the parse is confined and still parses"
