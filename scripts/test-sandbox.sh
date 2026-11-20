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
# tries the ways around a network policy matched on the build's cgroup (raw
# packets, mounting, the kernel keyring), plants links and settings for host
# root to follow out of the copy, and leaves an /etc/profile.d script that
# tries the same ways around from the build's container, where root runs it.
# There, root may mount only in a user namespace of its own, and neither it
# nor such a mount may make a read-only bind writable. A package installed
# with -I tries the same mounts from a container whose root keeps
# CAP_SYS_ADMIN and runs outside any sandbox, which only the container's own
# filter keeps from mounting.
#
# It also covers the worker's ssh-agents, one per key, which every build can
# reach by their fixed paths, being the build user's own: a build not offered
# a key must be refused its agent -- while another build is using it -- and a
# build offered one key must reach its agent and not another key's. The
# agents' directory is a protected path, and the refusal is Landlock's
# `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` (ABI 9, Linux 7.1); on an older kernel the
# sandbox leaves sockets alone, and those checks are reported as skipped.
# The worker holds named keys only where the sandbox, asked as the build user,
# says it would refuse them: with the policy at 0600 root, which the build
# user cannot read, it must answer `bad policy`, so the worker would hold no
# named key.
#
# Of the worker-like binds the build is given, the containers before it must
# get only the job's package cache; stock makechrootpkg's upgrade gets them all.
#
# And vsock, which no network namespace confines: a listener on the loopback
# CID, where the kernel has one, which the PKGBUILD on the host, the
# scriptlet, root at the build's start and the build itself try to reach --
# and the server's parse profile, which runs a PKGBUILD too.
#
# Last, with the protected-path list unreadable to the build user, the
# sandbox must refuse the build outright rather than read it as no list.
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
 && pacman -Syu --noconfirm --needed base-devel devtools git sudo python openssh \
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
# $1 = "confined" | "unconfined" | "offered" (confined, and offered the agent)
set -u
mode=$1
mount -t tmpfs tmpfs /run 2>/dev/null || true

# The legs differ in which script builds -- the patched copy, as the worker
# runs it, or the stock one from /usr/bin -- and in whether the build is
# offered the agent.
if [[ $mode == unconfined ]]; then
    build=(/usr/bin/makechrootpkg)
else
    build=(/usr/lib/aurcache/bin/aurcache-build "--interface=$(/usr/lib/aurcache/bin/aurcache-build --interface)")
fi

mkdir -p /srv && cd /srv && rm -rf upstream.git && git init -q --bare upstream.git
t=$(mktemp -d) && cd "$t" && git init -q . && git config user.email a@b && git config user.name a
echo hi > f.txt && git add . && git commit -qm init && git push -q /srv/upstream.git HEAD:master

rm -rf /shared /job /srv/canary /srv/ro-bind && mkdir -p /shared/{gnupg,srcdest-other,work-other} /job/{srcdest,pkg} /chroot
echo legit > /shared/srcdest-other/victim-source
# Bound read-only into the build (-D), as an operator's mount would be.
mkdir -p /srv/ro-bind && touch /srv/ro-bind/intact
# What the scriptlet's links and settings point host root at.
mkdir -p /srv/canary && echo intact > /srv/canary/victim && touch /srv/canary/host-only

# Secrets the worker actually holds, in the location the shipped protected-path
# list names. Owned by the build user on purpose: the point is that Landlock
# denies the read even when file permissions would allow it.
mkdir -p /var/lib/aurcache-worker/ssh
echo SSH-KEY-MATERIAL > /var/lib/aurcache-worker/ssh/id_ed25519
echo MTLS-KEY-MATERIAL > /var/lib/aurcache-worker/worker-key.pem
# The worker's ssh-agents, as the worker starts them: the build user's own,
# one per key, each at its fixed path and holding its key -- the default, and
# a named one only some packages may use.
mkdir -p /var/lib/aurcache-worker/agent/{default,other}
chown -R builder: /var/lib/aurcache-worker
# A secret a deployment keeps under /run, protected by adding it to the
# shipped list. The walk reaches /var on its way to the worker's directory,
# and there meets /var/run, a link to /run.
mkdir -p /run/aurcache-secret && echo RUN-KEY-MATERIAL > /run/aurcache-secret/key
chown -R builder: /run/aurcache-secret
grep -qx /run/aurcache-secret /etc/aurcache/sandbox-protected \
    || echo /run/aurcache-secret >> /etc/aurcache/sandbox-protected
agent_sock=/var/lib/aurcache-worker/agent/default/agent.sock
for name in default other; do
    sock=/var/lib/aurcache-worker/agent/$name/agent.sock
    sudo -u builder ssh-agent -a "$sock" >/dev/null
    sudo -u builder ssh-keygen -q -t ed25519 -N '' -f "/tmp/agent-key-$name"
    sudo -u builder env SSH_AUTH_SOCK="$sock" ssh-add -q "/tmp/agent-key-$name"
done
# Another build using the agent all the while, with its socket in its
# environment -- where /proc shows it to every process of the build user.
sudo -u builder env SSH_AUTH_SOCK="$agent_sock" \
    sh -c 'while ssh-add -l >/dev/null; do sleep 0.2; done' &
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
if [[ $mode != unconfined ]]; then
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
# What root in a container could use: the ways around a network policy on
# the build's cgroup, and a device node, which opens the host's device where
# the container has no device policy. Each is recorded in $1 as allowed or
# refused; the keyring's refusal is recorded by its reason, which says whose
# filter refused it.
cat > escapes <<'ESCAPES'
#!/bin/bash
mkdir -p "$1" && cd "$1" || exit 1
try() {
    local name=$1
    shift
    if "$@" >/dev/null 2>&1; then echo allowed; else echo refused; fi > "$name"
}
try raw perl -MSocket -e 'socket(my $s, 17, SOCK_RAW, 0) or exit 1'
try mount mount -t tmpfs aurcache-netprobe /mnt
try userns-mount unshare -rm mount -t tmpfs aurcache-netprobe /mnt
# The build's read-only bind (-D), where the container has one: made
# writable from a user namespace, where its flags are locked, then by root,
# which would leave it writable for the first.
if mountpoint -q /aurcache-ro; then
    try userns-remount-ro unshare -rm sh -c \
        'mount -o remount,bind,rw /aurcache-ro && touch /aurcache-ro/by-userns'
    try remount-ro sh -c 'mount -o remount,bind,rw /aurcache-ro && touch /aurcache-ro/by-root'
else
    echo absent | tee remount-ro > userns-remount-ro
fi
try mknod mknod /dev/aurcache-netprobe c 1 3
aurcache-netprobe-vsock > vsock
if out=$(keyctl add user aurcache-netprobe 1 @s 2>&1); then echo allowed; else echo "${out##*: }"; fi > keyring
ESCAPES
# reached, unreachable (a socket, but no listener) or refused (no socket).
cat > vsock <<'VSOCK'
#!/bin/bash
perl -MSocket -e 'socket(my $s, 40, SOCK_STREAM, 0) or exit 2;
    connect($s, pack("SSLLCx3", 40, 0, 7778, 1, 0)) or exit 1'
case $? in 0) echo reached ;; 1) echo unreachable ;; *) echo refused ;; esac
VSOCK
cat > netprobe.install <<'INSTALL'
post_install() {
    aurcache-netprobe-escapes /var/lib/aurcache-netprobe/escapes
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
    # Root sources this as the build's container starts.
    echo 'if [ "$(id -u)" = 0 ]; then aurcache-netprobe-escapes /startdir/root-escapes; fi' \
        > /etc/profile.d/aurcache-netprobe.sh
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
printf 'pkgname=aurcache-netprobe\npkgver=1\npkgrel=1\narch=(any)\ninstall=netprobe.install\npackage() { install -Dm755 "$startdir/escapes" "$pkgdir/usr/bin/aurcache-netprobe-escapes"; install -Dm755 "$startdir/vsock" "$pkgdir/usr/bin/aurcache-netprobe-vsock"; }\n' > PKGBUILD
chown -R builder: . && sudo -u builder makepkg >/dev/null 2>&1
cp /job/netprobe/*.pkg.tar.zst /srv/aurcache/
repo-add -q /srv/aurcache/aurcache.db.tar.gz /srv/aurcache/aurcache-netprobe-*.pkg.tar.zst
# Installed with -I: makechrootpkg runs `pacman -U` in a container of its own,
# whose root keeps CAP_SYS_ADMIN and is in no sandbox.
mkdir -p /job/mountprobe && cd /job/mountprobe
cat > mountprobe.install <<'INSTALL'
post_install() {
    mkdir -p /var/lib/aurcache-mountprobe && cd /var/lib/aurcache-mountprobe || return
    if mount -t tmpfs aurcache-mountprobe /mnt 2>/dev/null; then echo allowed; else echo refused; fi > mount
    if unshare -rm mount -t tmpfs aurcache-mountprobe /mnt 2>/dev/null
    then echo allowed; else echo refused; fi > userns-mount
}
INSTALL
printf 'pkgname=aurcache-mountprobe\npkgver=1\npkgrel=1\narch=(any)\ninstall=mountprobe.install\npackage() { :; }\n' > PKGBUILD
chown -R builder: . && sudo -u builder makepkg >/dev/null 2>&1
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
# vsock on the loopback CID, where the kernel has a loopback transport. What
# this kernel offers decides what the open legs can show.
python3 - <<'LISTEN' &
import socket
status = open("/tmp/vsock-kernel", "w", buffering=1)
try:
    s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
except OSError:
    status.write("none\n"); raise SystemExit
try:
    s.bind((1, 7778)); s.listen()  # VMADDR_CID_LOCAL
except OSError:
    status.write("no-loopback\n"); raise SystemExit
status.write("loopback\n")
heard = open("/tmp/heard-vsock", "a", buffering=1)
while True:
    s.accept()[0].close(); heard.write("vsock\n")
LISTEN
touch /tmp/heard /tmp/heard-vsock
for _ in {1..50}; do [[ -s /tmp/vsock-kernel ]] && break; sleep 0.1; done

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
    aurcache-netprobe-vsock > "$startdir/vsock"
}
BUILD
if [[ $mode != unconfined ]]; then
    # As the worker hands them over (aurcache_worker::build_deps).
    printf 'aurcache-netprobe\ntree\n' > /srv/build-deps
    confs+=(AURCACHE_BUILD_DEPS=/srv/build-deps)
fi

mkdir -p /srv/operator /srv/agent /srv/known-hosts /job/pkgcache
echo licence > /srv/operator/licence
binds=(-d /srv/operator:/opt/operator -D /srv/agent:/srv/agent
    -D /srv/known-hosts:/srv/known-hosts -d /job/pkgcache:/var/cache/pacman/pkg)
mv /usr/local/bin/systemd-nspawn /usr/local/bin/systemd-nspawn.recorded
cat > /usr/local/bin/systemd-nspawn <<'RECORD'
#!/bin/bash
name=
given=()
for a; do
    case $a in
        --machine=*) name=${a#--machine=} ;;
        --bind=*|--bind-ro=*|--tmpfs=*) given+=("$a") ;;
    esac
done
[[ -z $name ]] || printf '%s\n' "${given[@]}" > "/tmp/nspawn-binds/$name"
exec /usr/local/bin/systemd-nspawn.recorded "$@"
RECORD
chmod +x /usr/local/bin/systemd-nspawn
rm -rf /tmp/nspawn-binds && mkdir /tmp/nspawn-binds

cd /job/pkg
if [[ $mode == offered ]]; then
    # As the worker runs a build it offers the default key: as root, naming
    # the build user, with that agent's socket in the environment.
    env SRCDEST=/job/srcdest SSH_AUTH_SOCK="$agent_sock" "${confs[@]}" "${build[@]}" -U builder \
        -c -r /chroot -l job1 \
        -D /srv/ro-bind:/aurcache-ro -I /job/mountprobe/aurcache-mountprobe-1-1-any.pkg.tar.zst \
        "${binds[@]}" -- --nocheck >/tmp/build.log 2>&1
else
    sudo -u builder env SRCDEST=/job/srcdest "${confs[@]}" "${build[@]}" -c -r /chroot -l job1 \
        -D /srv/ro-bind:/aurcache-ro -I /job/mountprobe/aurcache-mountprobe-1-1-any.pkg.tar.zst \
        "${binds[@]}" -- --nocheck >/tmp/build.log 2>&1
fi
echo "build_exit=$?"
# The cache's sources in order: the job's must follow arch-nspawn's own bind
# of the same target to override it.
for c in sync-databases upgrade-packages download-dependencies install-dependencies; do
    given=$(cat /tmp/nspawn-binds/*-"$c".* 2>/dev/null)
    echo "${c}_ran=$([[ -n $given ]] && echo yes || echo no)"
    echo "${c}_operator=$(grep -q ':/opt/operator$' <<<"$given" && echo bound || echo absent)"
    echo "${c}_agent=$(grep -q '/srv/agent' <<<"$given" && echo bound || echo absent)"
    echo "${c}_known_hosts=$(grep -q '/srv/known-hosts' <<<"$given" && echo bound || echo absent)"
    echo "${c}_ro_bind=$(grep -q ':/aurcache-ro$' <<<"$given" && echo bound || echo absent)"
    echo "${c}_cache=$(grep '^--bind' <<<"$given" | awk '{
        sub(/^--bind(-ro)?=/, ""); n = split($0, p, ":")
        t = n > 1 ? p[2] : p[1]; s = p[1]; sub(/\/$/, "", t); sub(/\/$/, "", s)
        if (t == "/var/cache/pacman/pkg") print s
    }' | paste -sd, -)"
done
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
echo "var_run=$(readlink -f /var/run)"
echo "runkey=$([ -s /job/pkg/stolen-run ] && echo read || echo blocked)"
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
echo "ro_bind=$(ls -A /srv/ro-bind | paste -sd, -)"
for e in mount userns-mount; do
    echo "admin_$e=$(cat "/chroot/job1/var/lib/aurcache-mountprobe/$e" 2>/dev/null || echo missing)"
done
# Whether this host lets a process without CAP_SYS_ADMIN, as root in the
# build's container is, mount in a user namespace of its own. Ubuntu's
# kernel.apparmor_restrict_unprivileged_userns takes the capabilities of such
# a namespace away, and user.max_user_namespaces=0 refuses one at all.
if sudo -u builder unshare -rm mount -t tmpfs aurcache-host /mnt >/dev/null 2>&1
then echo host_userns_mount=allowed; else echo host_userns_mount=refused; fi
echo "vsock_kernel=$(cat /tmp/vsock-kernel 2>/dev/null || echo missing)"
echo "heard_vsock=$(sort -u /tmp/heard-vsock | paste -sd, -)"
# The server's parse sources a PKGBUILD as well: as `aurcache-parse` when the
# sandbox is started as root (the server images), and as the caller under
# Landlock alone otherwise (a native server). The same PKGBUILD sourced
# without it is the control. After the listener's count is taken: the
# control reaches it.
id aurcache-parse >/dev/null 2>&1 || useradd -r -M -s /usr/bin/nologin aurcache-parse
mkdir -p /srv/parse && cat > /srv/parse/PKGBUILD <<'PARSE'
perl -MSocket -e 'socket(my $s, 40, SOCK_STREAM, 0) or exit 2;
    connect($s, pack("SSLLCx3", 40, 0, 7778, 1, 0)) or exit 1'
case $? in 0) echo reached ;; 1) echo unreachable ;; *) echo refused ;; esac
PARSE
source_it=(bash -c 'source "$1"' parse)
parse_bare=$("${source_it[@]}" /srv/parse/PKGBUILD)
parse_root=$(aurcache-sandbox parse --pkgbuild /srv/parse/PKGBUILD -- "${source_it[@]}" 2>/dev/null)
parse_user=$(sudo -u builder aurcache-sandbox parse --pkgbuild /srv/parse/PKGBUILD \
    -- "${source_it[@]}" 2>/dev/null)
echo "host_vsock=$([[ -s /job/pkg/host-vsock ]] && sort -u /job/pkg/host-vsock | paste -sd, - || echo missing)"
echo "build_vsock=$(cat /job/pkg/vsock 2>/dev/null || echo missing)"
echo "parse_bare_vsock=${parse_bare:-missing}"
echo "parse_root_vsock=${parse_root:-missing}"
echo "parse_user_vsock=${parse_user:-missing}"
for e in raw mount userns-mount remount-ro userns-remount-ro mknod keyring vsock; do
    echo "install_$e=$(cat "/chroot/job1/var/lib/aurcache-netprobe/escapes/$e" 2>/dev/null || echo missing)"
    echo "build_root_$e=$(cat "/job/pkg/root-escapes/$e" 2>/dev/null || echo missing)"
done
[[ -s /job/pkg/sudo ]] || tail -n 40 /tmp/build.log | sed 's/^/log: /'
echo "agent=$([ -s /job/pkg/agent-reached ] && echo reached || echo blocked)"
echo "other_agent=$([ -s /job/pkg/other-agent-reached ] && echo reached || echo blocked)"
# Whether this kernel's Landlock can restrict UNIX sockets at all (ABI >= 9).
abi=$(python3 -c 'import ctypes; print(ctypes.CDLL(None).syscall(444, None, 0, 1))')
echo "landlock_abi=$abi"
# What the worker asks before it holds named keys, as it asks it.
capabilities() {
    sudo -u builder aurcache-sandbox --capabilities --agent-dir /var/lib/aurcache-worker/agent |
        paste -sd,
}
echo "capabilities=$(capabilities)"
# Judged where it really is: a directory spelled beneath the agents' one but
# elsewhere is not protected.
mkdir -p /var/lib/aurcache-worker/elsewhere
echo "dotdot_capabilities=$(sudo -u builder aurcache-sandbox --capabilities \
    --agent-dir /var/lib/aurcache-worker/agent/../elsewhere | cut -d' ' -f1-5)"

if [[ $mode == confined ]]; then
    # The policy where the build user cannot read it: the sandbox reads it as
    # that user, and must refuse the build before any of its code runs rather
    # than take it for a policy that protects nothing. Asked as that user
    # whether it keeps builds from the agents, it must answer `bad policy`.
    chmod 600 /etc/aurcache/sandbox-protected
    echo "unreadable_capabilities=$(capabilities)"
    mkdir -p /job/pkg2 && cp /fixture/PKGBUILD /job/pkg2/PKGBUILD && chown -R builder: /job/pkg2
    cd /job/pkg2
    sudo -u builder env SRCDEST=/job/srcdest "${confs[@]}" "${build[@]}" -c -r /chroot -l job2 \
        -- --nocheck >/tmp/build2.log 2>&1
    rc=$?
    echo "unreadable_build=$( ((rc)) && echo failed || echo built)"
    echo "unreadable_refusal=$(grep -q 'cannot read the protected-path list' /tmp/build2.log &&
        echo refused || echo missing)"
    echo "unreadable_ssh=$([ -s /job/pkg2/stolen-ssh ] && echo read || echo blocked)"
    chmod 644 /etc/aurcache/sandbox-protected
fi
EOF

docker build -q -t "$IMAGE" "$WORKDIR" >/dev/null

run() { docker run --rm --privileged --tmpfs /run "$IMAGE" /probe.sh "$1"; }

echo "==> leg 1/3: WITHOUT the sandbox (the fixture must escape)"
unconfined=$(run unconfined) || { echo "$unconfined"; exit 1; }
echo "$unconfined" | sed 's/^/    /'

echo "==> leg 2/3: WITH the sandbox (the fixture must be contained)"
confined=$(run confined) || { echo "$confined"; exit 1; }
echo "$confined" | sed 's/^/    /'

echo "==> leg 3/3: WITH the sandbox, offered the agent (it must still reach it)"
offered=$(run offered) || { echo "$offered"; exit 1; }
echo "$offered" | sed 's/^/    /'

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
check "unconfined: reads a secret under /run" read  "$(get runkey   "$unconfined")"
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
check "unconfined: root may open a packet socket" allowed "$(get install_raw "$unconfined")"
check "unconfined: root may mount"           allowed "$(get install_mount "$unconfined")"
check "unconfined: root may mount in a user namespace" allowed "$(get install_userns-mount "$unconfined")"
check "unconfined: root may make a device node" allowed "$(get install_mknod "$unconfined")"
check "unconfined: root may make a read-only bind writable" allowed "$(get install_remount-ro "$unconfined")"
check "unconfined: a user namespace may not" refused "$(get install_userns-remount-ro "$unconfined")"
check "unconfined: -I's root may mount"      allowed "$(get admin_mount "$unconfined")"
check "unconfined: -I's root may mount in a user namespace" allowed "$(get admin_userns-mount "$unconfined")"
check "unconfined: root writes through the read-only bind" by-root,intact "$(get ro_bind "$unconfined")"
check "unconfined: root may use the keyring" allowed "$(get install_keyring "$unconfined")"
check "unconfined: reaches the ssh-agent"  reached "$(get agent    "$unconfined")"
check "unconfined: reaches another key's agent" reached "$(get other_agent "$unconfined")"
# Stock makechrootpkg gives its upgrade every bind, so the record sees them.
check "unconfined: upgrade given the operator's mount" bound "$(get upgrade-packages_operator "$unconfined")"
check "unconfined: upgrade given the agent's directory" bound "$(get upgrade-packages_agent "$unconfined")"
check "unconfined: upgrade given the known_hosts directory" bound "$(get upgrade-packages_known_hosts "$unconfined")"
check "unconfined: upgrade given the read-only bind" bound "$(get upgrade-packages_ro_bind "$unconfined")"
# With it, every write is denied and the build still succeeds.
check "confined:   keyring untouched"       blocked "$(get gnupg   "$confined")"
check "confined:   other srcdest intact"    legit   "$(get srcdest  "$confined")"
check "confined:   other workdir untouched" blocked "$(get workdir  "$confined")"
check "confined:   ssh key unreadable"     blocked "$(get sshkey   "$confined")"
check "confined:   mTLS identity unreadable" blocked "$(get mtlskey "$confined")"
check "confined:   /var/run links to /run"  /run    "$(get var_run  "$confined")"
check "confined:   secret under /run unreadable" blocked "$(get runkey "$confined")"
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
for c in sync-databases upgrade-packages download-dependencies install-dependencies; do
    check "confined:   $c ran"                      yes    "$(get "${c}_ran" "$confined")"
    check "confined:   $c: no operator mount"       absent "$(get "${c}_operator" "$confined")"
    check "confined:   $c: no agent directory"      absent "$(get "${c}_agent" "$confined")"
    check "confined:   $c: no known_hosts directory" absent "$(get "${c}_known_hosts" "$confined")"
    check "confined:   $c: no read-only bind"       absent "$(get "${c}_ro_bind" "$confined")"
    check "confined:   $c: job cache over arch-nspawn's" /var/cache/pacman/pkg,/job/pkgcache "$(get "${c}_cache" "$confined")"
done
# Neither the install nor root in the build's container gets around a
# network policy on the build's cgroup. The install's keyring is refused by
# the sandbox's own filter, the build's by nspawn's.
for where in install build_root; do
    check "confined:   $where: no packet socket" refused "$(get "${where}_raw" "$confined")"
    check "confined:   $where: no mount"         refused "$(get "${where}_mount" "$confined")"
    check "confined:   $where: no device node"   refused "$(get "${where}_mknod" "$confined")"
done
# A container whose root keeps CAP_SYS_ADMIN refuses every mount call: -I's,
# outside any sandbox, shows the filter does, and the install's refuses them
# too. The build's, without CAP_SYS_ADMIN, is left them for a user namespace
# of its own, where the read-only bind stays read-only -- where the host
# allows such a namespace its capabilities at all.
check "confined:   -I's container: no mount"  refused "$(get admin_mount "$confined")"
check "confined:   -I's container: no mount in a user namespace" refused "$(get admin_userns-mount "$confined")"
check "confined:   install: no mount in a user namespace" refused "$(get install_userns-mount "$confined")"
check "confined:   build_root: read-only bind stays so" refused "$(get build_root_remount-ro "$confined")"
if [[ $(get host_userns_mount "$unconfined") == allowed ]]; then
    check "confined:   build_root: mounts in a user namespace" allowed "$(get build_root_userns-mount "$confined")"
    check "confined:   build_root: even from a user namespace" refused "$(get build_root_userns-remount-ro "$confined")"
else
    for what in "mounts in a user namespace" "even from a user namespace"; do
        echo "    skip confined:   build_root: $what: this host gives a process without" \
            "CAP_SYS_ADMIN no mount in a user namespace (kernel.apparmor_restrict_unprivileged_userns," \
            "user.max_user_namespaces)"
    done
fi
check "confined:   read-only bind untouched" intact "$(get ro_bind "$confined")"
check "confined:   install: no keyring"      "Function not implemented" "$(get install_keyring "$confined")"
check "confined:   build_root: no keyring"   "Operation not permitted" "$(get build_root_keyring "$confined")"
# The sandbox says it restricts UNIX sockets, which the worker requires
# before it holds named keys -- where the kernel can.
if (( $(get landlock_abi "$confined") >= 9 )); then
    check "sandbox:    reports resolve-unix"    resolve-unix "$(get capabilities "$confined")"
    check "sandbox:    judges the directory where it really is" \
        "no-resolve-unix: policy misses the agents" "$(get dotdot_capabilities "$confined")"
else
    check "sandbox:    reports the kernel cannot" "no-resolve-unix: kernel" \
        "$(get capabilities "$confined" | cut -d' ' -f1-2)"
fi
# With the policy unreadable to the build user, whatever the kernel, the
# sandbox answers that every build is refused, so the worker would hold no
# named key.
check "unreadable: sandbox says bad policy" "no-resolve-unix: bad policy" \
    "$(get unreadable_capabilities "$confined" | cut -d' ' -f1-3)"

# Offered the agent, the build reaches it and builds.
check "offered:    reaches the ssh-agent"   reached "$(get agent    "$offered")"
check "offered:    build still succeeds"    0       "$(get build_exit "$offered")"
# Not offered a key, the build is refused its agent, and offered one, it is
# refused every other -- where the kernel can say no.
if (( $(get landlock_abi "$confined") >= 9 )); then
    check "confined:   ssh-agent refused"       blocked "$(get agent    "$confined")"
    check "confined:   another key's agent refused" blocked "$(get other_agent "$confined")"
    check "offered:    another key's agent refused" blocked "$(get other_agent "$offered")"
else
    for what in "confined:   ssh-agent refused" "confined:   another key's agent refused" \
        "offered:    another key's agent refused"; do
        echo "    skip $what (Landlock ABI $(get landlock_abi "$confined") < 9)"
    done
fi
# With the policy unreadable to the build user: the build is refused before
# any of its code runs.
check "unreadable: build refused"           failed  "$(get unreadable_build "$confined")"
check "unreadable: by the sandbox"          refused "$(get unreadable_refusal "$confined")"
check "unreadable: ssh key unreadable"      blocked "$(get unreadable_ssh "$confined")"

# vsock: the open leg shows what this kernel lets a socket do, and the
# confined leg refuses the socket itself, everywhere a PKGBUILD's code runs.
# The host-side runs and the build are held by the sandbox's filters; root
# in the build's container too, by the one every container is started under;
# the install by the offline profile's own.
case $(get vsock_kernel "$unconfined") in
    loopback) open=reached ;;
    no-loopback) open=unreachable ;;
    *) open= ;;
esac
if [[ -n $open ]]; then
    check "unconfined: host-side PKGBUILD opens vsock" "$open" "$(get host_vsock "$unconfined")"
    check "unconfined: scriptlet opens vsock"      "$open" "$(get install_vsock "$unconfined")"
    check "unconfined: build opens vsock"          "$open" "$(get build_vsock "$unconfined")"
    [[ $open == reached ]] &&
        check "unconfined: vsock listener hears it" vsock "$(get heard_vsock "$unconfined")"
    check "confined:   host-side PKGBUILD: no vsock" refused "$(get host_vsock "$confined")"
    check "confined:   install: no vsock"            refused "$(get install_vsock "$confined")"
    check "confined:   build_root: no vsock"         refused "$(get build_root_vsock "$confined")"
    check "confined:   build: no vsock"              refused "$(get build_vsock "$confined")"
    check "confined:   vsock listener hears nothing" ""     "$(get heard_vsock "$confined")"
    # The parse profile, as both kinds of server run it, beside the control.
    check "unsandboxed parse opens vsock"          "$open"  "$(get parse_bare_vsock "$confined")"
    check "parse as root: no vsock"                refused  "$(get parse_root_vsock "$confined")"
    check "parse unprivileged: no vsock"           refused  "$(get parse_user_vsock "$confined")"
else
    echo "    skip vsock: this kernel has no AF_VSOCK ($(get vsock_kernel "$unconfined"))"
fi

if (( fail )); then
    echo "==> FAILED"
    (( KEEP )) && echo "    test image kept as $IMAGE"
    exit 1
fi
echo "==> PASSED"
