#!/usr/bin/env bash
# Build isolation regression test.
#
# A PKGBUILD is executed on the worker, outside any chroot, during source
# download — twice: by `makepkg --verifysource` and by makechrootpkg's direct
# `source PKGBUILD`. Unconfined, that lets one build write another's cached
# sources, the shared GnuPG keyring, and a concurrent job's PKGBUILD.
#
# This test asserts BOTH polarities, because an allow-list fails open: proving
# the denials only means something if the same fixture demonstrably escapes
# without the sandbox. A widened allow-list that reopens a hole turns the
# "unconfined" leg green in the confined run, and this test red.
#
# The same two legs check the patched script's other duty: the build's system
# upgrade never sees AURCache's repository, where a package's `replaces=`
# would take effect. Stock `makechrootpkg -u` takes a bait from it; the
# patched script, given the worker's two configurations, must not.
#
# And the build's dependencies: one from that repository has an `.install`
# scriptlet that tries TCP and UDP to a listener beside the chroot. Stock
# makechrootpkg has makepkg install it through `sudo pacman`, with network;
# the patched script installs it before the build under
# `aurcache-sandbox offline`, where the scriptlet reaches nothing -- and the
# build that follows cannot use sudo at all. The scriptlet, as root, also
# plants links and settings for host root to follow out of the copy.
#
# Usage: scripts/test-sandbox.sh [--keep]
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib/common.sh
. scripts/lib/common.sh

KEEP=0
[[ ${1:-} == --keep ]] && KEEP=1
IMAGE=aurcache-sandbox-test
WORKDIR=$(mktemp -d)
trap '(( KEEP )) || rm -rf "$WORKDIR"' EXIT

echo "==> building test image (Arch + devtools + sandbox)"
sandbox=$(cargo_executable aurcache-sandbox build --release \
    --manifest-path backend/Cargo.toml -p aurcache-sandbox)
cp "$sandbox" "$WORKDIR/"
cp docker/nspawn-wrapper.sh packaging/patch-makechrootpkg.py packaging/sandbox-protected "$WORKDIR/"
cp docker/testpkg/hostile-fixture/PKGBUILD "$WORKDIR/"

cat > "$WORKDIR/Dockerfile" <<'EOF'
FROM archlinux/archlinux:latest
RUN sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
 && pacman -Syu --noconfirm --needed base-devel devtools git sudo python \
 && pacman-key --init && pacman-key --populate && systemd-machine-id-setup \
 && useradd -m builder && echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder
COPY aurcache-sandbox /usr/local/bin/aurcache-sandbox
COPY nspawn-wrapper.sh /usr/local/bin/systemd-nspawn
COPY patch-makechrootpkg.py /usr/local/bin/patch-makechrootpkg
# The same list the real images ship, so this test exercises the shipped
# policy rather than a copy of it that could drift.
COPY sandbox-protected /etc/aurcache/sandbox-protected
COPY PKGBUILD /fixture/PKGBUILD
RUN chmod +x /usr/local/bin/aurcache-sandbox /usr/local/bin/systemd-nspawn \
      /usr/local/bin/patch-makechrootpkg \
 && /usr/local/bin/patch-makechrootpkg \
 && /usr/lib/aurcache/bin/aurcache-build --interface >/dev/null
COPY probe.sh /probe.sh
RUN chmod +x /probe.sh
EOF

cat > "$WORKDIR/probe.sh" <<'EOF'
#!/bin/bash
# $1 = "confined" | "unconfined"
set -u
mode=$1
mount -t tmpfs tmpfs /run 2>/dev/null || true

# The only difference between the legs is which script builds: the patched
# copy, as the worker runs it, or the stock one from /usr/bin.
if [[ $mode == confined ]]; then
    build=(/usr/lib/aurcache/bin/aurcache-build "--interface=$(/usr/lib/aurcache/bin/aurcache-build --interface)")
else
    build=(/usr/bin/makechrootpkg)
fi

mkdir -p /srv && cd /srv && rm -rf upstream.git && git init -q --bare upstream.git
t=$(mktemp -d) && cd "$t" && git init -q . && git config user.email a@b && git config user.name a
echo hi > f.txt && git add . && git commit -qm init && git push -q /srv/upstream.git HEAD:master

rm -rf /shared /job /srv/canary && mkdir -p /shared/{gnupg,srcdest-other,work-other} /job/{srcdest,pkg} /chroot
echo legit > /shared/srcdest-other/victim-source
# What the scriptlet's links and settings point host root at.
mkdir -p /srv/canary && echo intact > /srv/canary/victim && touch /srv/canary/host-only

# Secrets the worker actually holds, in the location the shipped protected-path
# list names. Owned by the build user on purpose: the point is that Landlock
# denies the read even when file permissions would allow it.
mkdir -p /var/lib/aurcache-worker/secrets
echo SSH-KEY-MATERIAL > /var/lib/aurcache-worker/secrets/id_ed25519
echo MTLS-KEY-MATERIAL > /var/lib/aurcache-worker/worker-key.pem
chown -R builder: /var/lib/aurcache-worker
cp /fixture/PKGBUILD /job/pkg/PKGBUILD
chown -R builder: /job /shared

[[ -d /chroot/root ]] || mkarchroot -C /etc/pacman.conf -M /etc/makepkg.conf /chroot/root base-devel git >/dev/null 2>&1

# The build's upgrade must not see AURCache's repository: pacman acts on
# `replaces=` only while upgrading. What a repository listed last can replace
# is an installed package no earlier one has (one since dropped from the
# official repositories), so the chroot gets one, and a local repository named
# like AURCache's gets a bait that replaces it.
for p in stale bait; do
    mkdir -p "/job/$p" && cd "/job/$p"
    printf 'pkgname=aurcache-%s\npkgver=1\npkgrel=1\narch=(any)\npackage() { :; }\n' "$p" > PKGBUILD
    [[ $p == bait ]] && printf '%s=(aurcache-stale)\n' provides conflicts replaces >> PKGBUILD
    chown -R builder: . && sudo -u builder makepkg >/dev/null 2>&1
done
cp /job/stale/aurcache-stale-1-1-any.pkg.tar.zst /chroot/root/stale.pkg.tar.zst
arch-nspawn /chroot/root pacman -U --noconfirm /stale.pkg.tar.zst >/dev/null 2>&1
mkdir -p /srv/aurcache && cp /job/bait/*.pkg.tar.zst /srv/aurcache/
repo-add -q /srv/aurcache/aurcache.db.tar.gz /srv/aurcache/*.pkg.tar.zst
section=$'\n[aurcache]\nSigLevel = Never\nServer = file:///srv/aurcache\n'
cp /etc/pacman.conf /srv/pacman-upgrade.conf
{ cat /etc/pacman.conf; printf '%s' "$section"; } > /srv/pacman-build.conf
# Both legs start from a base that lists the repository, as one made from a
# configuration that did; only the invocation differs.
cp /srv/pacman-build.conf /chroot/root/etc/pacman.conf
if [[ $mode == confined ]]; then
    # As the worker runs it: sync with the build's configuration, upgrade with
    # the upgrade's.
    confs=(AURCACHE_PACMAN_CONF=/srv/pacman-build.conf AURCACHE_UPGRADE_CONF=/srv/pacman-upgrade.conf)
else
    confs=()
    build+=(-u)
fi

# The dependency, from the same repository: its scriptlet records whether
# TCP and UDP reached the listener, and the listener what it heard. It also
# tries to leave its network namespace for any other one it can see, and
# records whether it still holds CAP_SYS_ADMIN and CAP_SYS_PTRACE, which
# such an escape takes.
mkdir -p /job/netprobe && cd /job/netprobe
cat > netprobe.install <<'INSTALL'
post_install() {
    # Links out of the copy, and settings that would have host root bind
    # /srv/canary into the build or run a command: all resolve inside the
    # container, and none may reach the host's /srv/canary.
    mkdir -p /srv/canary
    printf '[safe]\n\tdirectory = *\n' > /srv/canary/gitconfig
    ln -sf /srv/canary/gitconfig /etc/gitconfig
    cp /etc/pacman.d/mirrorlist /srv/canary/mirrorlist
    ln -sf /srv/canary/mirrorlist /etc/pacman.d/mirrorlist
    sed -i 's|^CacheDir *= *|CacheDir = /srv/canary/ |' /etc/pacman.conf
    echo 'CARCH=x86_64$(touch /srv/canary/evaled 2>/dev/null; :)' >> /etc/makepkg.conf
    rm -rf /srcpkgdest && ln -s /srv/canary /srcpkgdest
    mkdir -p /var/lib/aurcache-netprobe && cd /var/lib/aurcache-netprobe
    if (exec 3<>/dev/tcp/127.0.0.1/7777) 2>/dev/null; then echo reached; else echo blocked; fi > tcp
    if (exec 3>/dev/udp/127.0.0.1/7777 && echo udp >&3) 2>/dev/null; then echo sent; else echo blocked; fi > udp
    awk '/^NoNewPrivs:/ {print $2}' /proc/self/status > nnp
    for ns in /proc/[0-9]*/ns/net; do
        if nsenter --net="$ns" bash -c 'exec 3<>/dev/tcp/127.0.0.1/7777' 2>/dev/null; then
            echo reached
            break
        fi
    done > escape
    [ -s escape ] || echo blocked > escape
    bnd=$(awk '/^CapBnd:/ {print $2}' /proc/self/status)
    echo "$(( 0x$bnd >> 21 & 1 ))$(( 0x$bnd >> 19 & 1 ))" > admin-ptrace
}
INSTALL
printf 'pkgname=aurcache-netprobe\npkgver=1\npkgrel=1\narch=(any)\ninstall=netprobe.install\npackage() { :; }\n' > PKGBUILD
chown -R builder: . && sudo -u builder makepkg >/dev/null 2>&1
cp /job/netprobe/*.pkg.tar.zst /srv/aurcache/
repo-add -q /srv/aurcache/aurcache.db.tar.gz /srv/aurcache/aurcache-netprobe-*.pkg.tar.zst
python3 - <<'LISTEN' &
import socket, threading
heard = open("/tmp/heard", "a", buffering=1)
def tcp():
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", 7777)); s.listen()
    while True:
        s.accept()[0].close(); heard.write("tcp\n")
def udp():
    s = socket.socket(type=socket.SOCK_DGRAM); s.bind(("127.0.0.1", 7777))
    while True:
        s.recv(64); heard.write("udp\n")
threading.Thread(target=udp, daemon=True).start()
tcp()
LISTEN
touch /tmp/heard
# The build depends on it, and on an official package, whose signature is
# checked by the confined install; and it tries to become root once it runs.
cat >> /job/pkg/PKGBUILD <<'BUILD'
makedepends=(aurcache-netprobe tree)
build() {
    grep '^NoNewPrivs:' /proc/self/status | awk '{print $2}' > "$startdir/nnp"
    if [ -e /srv/canary/host-only ]; then echo bound; else echo absent; fi > "$startdir/canary-bind"
    if sudo -n pacman -V >/dev/null 2>&1; then echo allowed; else echo refused; fi > "$startdir/sudo"
    if sudo -n pacman -S --asdeps --needed --noconfirm aurcache-netprobe >/dev/null 2>&1
    then echo allowed; else echo refused; fi > "$startdir/sudo-install"
}
BUILD
if [[ $mode == confined ]]; then
    # As the worker hands them over (aurcache_worker::build_deps).
    printf 'aurcache-netprobe\ntree\n' > /srv/build-deps
    confs+=(AURCACHE_BUILD_DEPS=/srv/build-deps)
fi

cd /job/pkg
sudo -u builder env SRCDEST=/job/srcdest "${confs[@]}" "${build[@]}" -c -r /chroot -l job1 -- --nocheck >/tmp/build.log 2>&1
echo "build_exit=$?"
echo "gnupg=$([ -e /shared/gnupg/EVIL_trustdb ] && echo written || echo blocked)"
echo "srcdest=$(cat /shared/srcdest-other/victim-source)"
echo "workdir=$([ -e /shared/work-other/EVIL_pkgbuild ] && echo written || echo blocked)"
# The payloads redirect into the (writable) package dir, so a non-empty file
# means the READ succeeded — this measures reads, not writes.
# Not a payload: a capability the sandbox must allow. Signature verification
# needs a temp file, and denying it breaks signed packages only.
echo "mktemp=$([ -s /job/pkg/mktemp-ok ] && echo works || echo broken)"
echo "sshkey=$([ -s /job/pkg/stolen-ssh ] && echo read || echo blocked)"
echo "mtlskey=$([ -s /job/pkg/stolen-mtls ] && echo read || echo blocked)"
# What the build's copy has installed under the stale package's name.
echo "upgrade=$(pacman -b /chroot/job1/var/lib/pacman -Qq aurcache-stale 2>&1)"
echo "dep=$(pacman -b /chroot/job1/var/lib/pacman -Qq aurcache-netprobe 2>&1)"
echo "scriptlet_tcp=$(cat /chroot/job1/var/lib/aurcache-netprobe/tcp 2>/dev/null || echo missing)"
echo "scriptlet_udp=$(cat /chroot/job1/var/lib/aurcache-netprobe/udp 2>/dev/null || echo missing)"
echo "scriptlet_nnp=$(cat /chroot/job1/var/lib/aurcache-netprobe/nnp 2>/dev/null || echo missing)"
echo "scriptlet_escape=$(cat /chroot/job1/var/lib/aurcache-netprobe/escape 2>/dev/null || echo missing)"
echo "scriptlet_admin_ptrace=$(cat /chroot/job1/var/lib/aurcache-netprobe/admin-ptrace 2>/dev/null || echo missing)"
# The sandbox names the layers it applied; both network ones must have.
echo "offline_layers=$(grep -o 'aurcache-sandbox offline: netns+seccomp' /tmp/build.log | head -n 1 | cut -d' ' -f3)"
echo "official=$(pacman -b /chroot/job1/var/lib/pacman -Qq tree 2>&1)"
echo "heard=$(sort -u /tmp/heard | paste -sd, -)"
echo "build_sudo=$(cat /job/pkg/sudo 2>/dev/null || echo missing)"
echo "build_sudo_install=$(cat /job/pkg/sudo-install 2>/dev/null || echo missing)"
echo "build_nnp=$(cat /job/pkg/nnp 2>/dev/null || echo missing)"
echo "sudoers=$([ -e /chroot/job1/etc/sudoers.d/builduser-pacman ] && echo present || echo absent)"
echo "canary=$(ls -A /srv/canary | paste -sd, -)"
echo "canary_victim=$(cat /srv/canary/victim 2>/dev/null || echo missing)"
echo "canary_bind=$(cat /job/pkg/canary-bind 2>/dev/null || echo missing)"
[[ -s /job/pkg/sudo ]] || tail -n 40 /tmp/build.log | sed 's/^/log: /'
EOF

docker build -q -t "$IMAGE" "$WORKDIR" >/dev/null

run() { docker run --rm --privileged --tmpfs /run "$IMAGE" /probe.sh "$1"; }

echo "==> leg 1/2: WITHOUT the sandbox (the fixture must escape)"
unconfined=$(run unconfined) || { echo "$unconfined"; exit 1; }
echo "$unconfined" | sed 's/^/    /'

echo "==> leg 2/2: WITH the sandbox (the fixture must be contained)"
confined=$(run confined) || { echo "$confined"; exit 1; }
echo "$confined" | sed 's/^/    /'

fail=0
check() { # name expected actual
    if [[ $3 == "$2" ]]; then echo "    ok   $1"; else echo "    FAIL $1: expected '$2', got '$3'"; fail=1; fi
}
get() { echo "$2" | grep "^$1=" | cut -d= -f2-; }

echo "==> assertions"
# Without the sandbox the fixture must actually escape, or the confined leg
# proves nothing.
check "unconfined: writes shared keyring"   written "$(get gnupg   "$unconfined")"
check "unconfined: writes other srcdest"    pwned   "$(get srcdest  "$unconfined")"
check "unconfined: writes other workdir"    written "$(get workdir  "$unconfined")"
check "unconfined: reads ssh key"          read    "$(get sshkey   "$unconfined")"
check "unconfined: reads mTLS identity"    read    "$(get mtlskey  "$unconfined")"
check "unconfined: upgrade takes the bait" aurcache-bait "$(get upgrade "$unconfined")"
check "unconfined: dependency installed"    aurcache-netprobe "$(get dep "$unconfined")"
check "unconfined: scriptlet reaches TCP"   reached "$(get scriptlet_tcp "$unconfined")"
check "unconfined: scriptlet sends UDP"     sent    "$(get scriptlet_udp "$unconfined")"
check "unconfined: listener hears both"     tcp,udp "$(get heard "$unconfined")"
check "unconfined: scriptlet may gain privileges" 0 "$(get scriptlet_nnp "$unconfined")"
check "unconfined: scriptlet reaches TCP through nsenter" reached "$(get scriptlet_escape "$unconfined")"
check "unconfined: scriptlet holds SYS_ADMIN, SYS_PTRACE" 11 "$(get scriptlet_admin_ptrace "$unconfined")"
check "unconfined: build can sudo pacman"   allowed "$(get build_sudo "$unconfined")"
check "unconfined: build can install"       allowed "$(get build_sudo_install "$unconfined")"
check "unconfined: host root follows a planted srcpkgdest" "" "$(get canary "$unconfined")"
# With it, every write is denied and the build still succeeds.
check "confined:   keyring untouched"       blocked "$(get gnupg   "$confined")"
check "confined:   other srcdest intact"    legit   "$(get srcdest  "$confined")"
check "confined:   other workdir untouched" blocked "$(get workdir  "$confined")"
check "confined:   ssh key unreadable"     blocked "$(get sshkey   "$confined")"
check "confined:   mTLS identity unreadable" blocked "$(get mtlskey "$confined")"
check "confined:   mktemp still works"     works   "$(get mktemp    "$confined")"
check "confined:   build still succeeds"    0       "$(get build_exit "$confined")"
check "confined:   upgrade never sees AURCache's repository" aurcache-stale "$(get upgrade "$confined")"
check "confined:   dependency installed"    aurcache-netprobe "$(get dep "$confined")"
check "confined:   scriptlet has no TCP"    blocked "$(get scriptlet_tcp "$confined")"
check "confined:   scriptlet has no UDP"    blocked "$(get scriptlet_udp "$confined")"
check "confined:   listener hears nothing"  ""      "$(get heard "$confined")"
check "confined:   scriptlet has no_new_privs" 1    "$(get scriptlet_nnp "$confined")"
check "confined:   scriptlet cannot nsenter out" blocked "$(get scriptlet_escape "$confined")"
check "confined:   SYS_ADMIN, SYS_PTRACE dropped" 00  "$(get scriptlet_admin_ptrace "$confined")"
check "confined:   install in netns and seccomp" netns+seccomp "$(get offline_layers "$confined")"
check "confined:   signed official dep installed" tree "$(get official "$confined")"
check "confined:   build cannot sudo pacman" refused "$(get build_sudo "$confined")"
check "confined:   build cannot install"    refused "$(get build_sudo_install "$confined")"
check "confined:   build has no_new_privs"  1       "$(get build_nnp "$confined")"
check "confined:   no sudoers entry"        absent  "$(get sudoers "$confined")"
# Nothing host root does after the install follows what it planted.
check "confined:   host's /srv/canary untouched" host-only,victim "$(get canary "$confined")"
check "confined:   host's /srv/canary/victim intact" intact "$(get canary_victim "$confined")"
check "confined:   build binds nothing the copy names" absent "$(get canary_bind "$confined")"

if (( fail )); then
    echo "==> FAILED"
    (( KEEP )) && echo "    test image kept as $IMAGE"
    exit 1
fi
echo "==> PASSED"
