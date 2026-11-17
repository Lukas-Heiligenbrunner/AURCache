#!/usr/bin/env bash
# Signed repository check: what pacman makes of what the server signs.
#
# The unit tests verify signatures with GnuPG; this asks pacman itself, in an
# Arch container with no network, against a repository the server's own code
# wrote (backend/aurcache-utils/tests/signed_repository.rs): one package the
# backfill signed, one published signed. It asserts that
#
#   1. a client that does not trust the key refuses the repository even with
#      `SigLevel = Optional TrustAll`, as the documentation warns;
#   2. with the key trusted the way the documentation says -- piped into
#      `pacman-key --add -` -- `SigLevel = Required` installs both packages;
#   3. a database that is not the one signed is refused;
#   4. a package whose signature is not over it is refused, by the package
#      signature alone (database signatures not checked).
#
# Usage: scripts/test-signed-repo.sh [--keep]
set -euo pipefail
cd "$(dirname "$0")/.."

KEEP=0
[[ ${1:-} == --keep ]] && KEEP=1
WORKDIR=$(mktemp -d)
cleanup() {
	if (( KEEP )); then
		echo "==> kept $WORKDIR"
	else
		rm -rf "$WORKDIR"
	fi
}
trap cleanup EXIT

echo "==> writing a signed repository"
AURCACHE_SIGNED_REPO_DIR="$WORKDIR" cargo test --manifest-path backend/Cargo.toml \
    -p aurcache-utils --test signed_repository -- --ignored --quiet

cat > "$WORKDIR/check.sh" <<'EOF'
#!/bin/bash
set -u
fail() { echo "FAIL: $*"; exit 1; }
FINGERPRINT=$(cat /w/fingerprint)
X=/srv/repo/x86_64
pacman-key --init >/dev/null 2>&1 || fail "pacman-key --init"
cp -a /w/repo /srv/repo

conf() {
	cat > /etc/pacman.conf <<CONF
[options]
Architecture = auto
SigLevel = Required DatabaseOptional
LocalFileSigLevel = Optional

[repo]
SigLevel = $1
Server = file:///srv/repo/\$arch
CONF
}

reset() {
	pacman -Rns --noconfirm signed-later signed-now >/dev/null 2>&1
	rm -rf /var/cache/pacman/pkg/* /var/lib/pacman/sync/repo.*
}

conf "Optional TrustAll"
if pacman -Sy --noconfirm signed-later >/tmp/out 2>&1; then
	cat /tmp/out
	fail "1: a key nobody trusts was accepted under Optional TrustAll"
fi
grep -q "is unknown" /tmp/out || { cat /tmp/out; fail "1: refused for another reason"; }
echo "ok 1: refused while the key is not trusted"
reset

# From a pipe, as the documented command and `aurcli repo config --install`
# feed it: `--add -`, since pacman-key refuses `--add` with no argument.
cat /w/key.asc | pacman-key --add - >/dev/null 2>&1 || fail "pacman-key --add -"
pacman-key --lsign-key "$FINGERPRINT" >/dev/null 2>&1 || fail "pacman-key --lsign-key"
conf "Required"
pacman -Sy --noconfirm signed-later signed-now >/tmp/out 2>&1 || { cat /tmp/out; fail "2: refused"; }
for name in signed-later signed-now; do
	[ -f "/usr/share/doc/$name/README" ] || fail "2: $name is not installed"
done
echo "ok 2: SigLevel = Required installs both packages"
reset

cp $X/repo.db.tar.gz /tmp/db
gzip -dc /tmp/db | gzip -9n > $X/repo.db.tar.gz
[ "$(sha256sum < /tmp/db)" != "$(sha256sum < $X/repo.db.tar.gz)" ] || fail "3: database unchanged"
if pacman -Sy --noconfirm >/tmp/out 2>&1; then
	cat /tmp/out
	fail "3: a database not matching its signature was accepted"
fi
grep -q "PGP signature" /tmp/out || { cat /tmp/out; fail "3: refused for another reason"; }
echo "ok 3: a database not matching repo.db.sig is refused"
cp /tmp/db $X/repo.db.tar.gz
reset

# signed-now's signature served for signed-later, beside it and in its entry.
cp $X/signed-now-1.0-1-any.pkg.tar.zst.sig $X/signed-later-1.0-1-any.pkg.tar.zst.sig
mkdir /tmp/db.d && bsdtar -xzf $X/repo.db.tar.gz -C /tmp/db.d
OTHER=$(awk '/^%PGPSIG%$/ { getline; print; exit }' /tmp/db.d/signed-now-1.0-1/desc)
awk -v sig="$OTHER" 'swap { print sig; swap = 0; next } /^%PGPSIG%$/ { swap = 1 } { print }' \
	/tmp/db.d/signed-later-1.0-1/desc > /tmp/desc
mv /tmp/desc /tmp/db.d/signed-later-1.0-1/desc
rm $X/repo.db.tar.gz
bsdtar -czf $X/repo.db.tar.gz -C /tmp/db.d signed-later-1.0-1 signed-now-1.0-1
conf "PackageRequired DatabaseNever"
if pacman -Sy --noconfirm signed-later >/tmp/out 2>&1; then
	cat /tmp/out
	fail "4: a package with another's signature was installed"
fi
grep -q "invalid or corrupted package (PGP signature)" /tmp/out || { cat /tmp/out; fail "4: refused for another reason"; }
echo "ok 4: a package whose signature is not over it is refused"
EOF

echo "==> checking it with pacman"
docker run --rm --network none -v "$WORKDIR:/w:ro" archlinux/archlinux:latest bash /w/check.sh
echo "==> signed repository check passed"
