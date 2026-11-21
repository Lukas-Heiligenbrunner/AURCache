#!/usr/bin/env bash
# The build agent, for real: `aurcache-build-agent build` in the builder image,
# under exactly the flags a worker gives a build container (user 0,
# no-new-privileges, every capability dropped but seven).
#
# It builds the agent as it ships (statically linked) and runs four legs:
#
#   1. docker/testpkg/hello builds: a report, measurements, and one package
#      handed back to the work directory's owner;
#   2. docker/testpkg/agent-hostile builds, and both while its dependencies
#      are read from it and from inside build() tries to become root, find
#      anything of root's to talk to, and write the report. Both polarities:
#      its dependencies are only there if root installed them before the
#      build. One of them, docker/testpkg/agent-thief, comes from a
#      repository of the test's own, which stands for AURCache's: the image
#      is upgraded without it, so a package there that would upgrade an
#      installed one is left alone. Then the same package, its
#      `--printsrcinfo` forged to name a package file and pacman's options:
#      root's pacman takes them as names it cannot find, and the build fails
#      before it starts;
#   3. a build is stopped half-way, once with SIGKILL and once with SIGTERM:
#      no process outlives it and no report is written;
#   4. where qemu-aarch64 is registered and the arm64 builder image can be
#      pulled, leg 1 again in an arm64 container with the same x86_64 agent.
#
# Usage: scripts/test-build-agent.sh [--keep]
#
#   --keep                keep the containers and the work directories
#   BUILDER_IMAGE         the image builds run in
#   AURCACHE_TEST_PREFIX  the prefix of every container this creates
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib/common.sh
. scripts/lib/common.sh

case ${1:-} in
    "") KEEP=0 ;;
    --keep) KEEP=1 ;;
    -h | --help)
        header_help "$0"
        exit 0
        ;;
    *)
        echo "usage: $0 [--keep]" >&2
        exit 2
        ;;
esac

PREFIX=${AURCACHE_TEST_PREFIX:-aurcache-test}
IMAGE=${BUILDER_IMAGE:-ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest}
# What serves the test's pacman repository: busybox's httpd.
REPO_SERVER_IMAGE=busybox:1.37
AGENT=/aurcache/bin/aurcache-build-agent
# aurcache_build_agent::layout::CAPABILITIES
CAPABILITIES=(CHOWN DAC_OVERRIDE FOWNER FSETID SETUID SETGID KILL)
# What a worker gives a build container, as Docker flags.
CONTAINER_FLAGS=(--user 0 --security-opt no-new-privileges --cap-drop=ALL "${CAPABILITIES[@]/#/--cap-add=}")
# The longest one build may take before the leg is given up.
BUILD_TIMEOUT=1800
# What the hostile fixture is told its source is, and the same seen from here.
FIXTURE_SOURCE="fixture::git+file:///srv/aurcache-agent-test/upstream.git"

WORK=$(mktemp -d)
CONTAINERS=()
FAIL=0

container() { echo "$PREFIX-agent-$1"; }

cleanup() {
    local name
    if ((KEEP)); then
        echo "==> kept: $WORK, and the containers $(container '*')"
        return
    fi
    for name in "${CONTAINERS[@]}"; do
        docker rm -f "$name" >/dev/null 2>&1 || true
    done
    # A build that was stopped never handed its directory back: it is root's.
    if ! rm -rf "$WORK" 2>/dev/null; then
        docker run --rm --name "$(container cleanup)" --user 0 -v "$WORK:/w" \
            --entrypoint /bin/sh "$IMAGE" -c 'find /w -mindepth 1 -delete' >/dev/null 2>&1 || true
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

check() { # name expected actual
    if [[ $3 == "$2" ]]; then
        echo "    ok   $1"
    else
        echo "    FAIL $1: expected '$2', got '$3'"
        FAIL=1
    fi
}

# Write a job into its work directory: the source archive and the spec.
#   prepare <leg> <fixture directory> <pkgbase> <arch> [option=value...]
prepare() {
    local leg=$1 fixture=$2 pkgbase=$3 arch=$4
    shift 4
    mkdir -p "$WORK/$leg/in" "$WORK/stage/$leg/$pkgbase"
    cp -r "$fixture/." "$WORK/stage/$leg/$pkgbase/"
    tar -czf "$WORK/$leg/in/source.tar.gz" -C "$WORK/stage/$leg" "$pkgbase"
    python3 - "$WORK/$leg/in/spec.json" "$pkgbase" "$arch" "$FIXTURE_SOURCE" \
        "interface=$INTERFACE" "$@" <<'EOF'
import json, sys

path, pkgbase, arch, fixture_source = sys.argv[1:5]
options = dict(option.split("=", 1) for option in sys.argv[5:])

# What the server renders for a job: its base pacman.conf for the
# architecture, and its own makepkg assignments after the operator's.
repos = {"x86_64": ["core", "extra", "multilib"]}.get(arch, ["core", "extra", "alarm", "aur"])
pacman_conf = (
    "[options]\nDisableSandbox\nSigLevel = Never\nHoldPkg = pacman glibc\n"
    f"Architecture = {arch}\n"
)
# What the image is upgraded with: the job's configuration as the server
# sends it.
upgrade_conf = pacman_conf + "".join(
    f"\n[{repo}]\nInclude = /etc/pacman.d/mirrorlist\n" for repo in repos
)
# What the build installs with. The test's repository, which has the hostile
# package's dependency, stands for AURCache's own, which a worker adds. It
# goes ahead of the official ones here, where AURCache's never is: pacman
# takes an installed package's upgrade from the first repository that has
# it, so this is where an upgrade that saw this repository would take the
# bait from.
if "repo" in options:
    pacman_conf += f"\n[thief]\nServer = http://{options['repo']}:8080\n"
pacman_conf += "".join(f"\n[{repo}]\nInclude = /etc/pacman.d/mirrorlist\n" for repo in repos)
# The first two are the agent's to decide, and must not win.
makepkg_conf = "PKGDEST=/tmp/not-here\nMAKEFLAGS=-j1\n"
if "vcs" in options:
    # The fixture's repository belongs to whoever runs this script, and git
    # will not serve a clone from someone else's without being told to.
    makepkg_conf += (
        "export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'\n"
    )
if "repo" in options:
    makepkg_conf += "AURCACHE_TEST_REPO=1\n"
if "linger" in options:
    makepkg_conf += f"AURCACHE_TEST_LINGER={options['linger']}\n"
if "forge" in options:
    makepkg_conf += "AURCACHE_TEST_FORGE=1\n"
makepkg_conf += "PACKAGER='AURCache <aurcache@localhost>'\nOPTIONS+=(!debug)\n"

spec = {
    "interface": int(options["interface"]),
    "pkgbase": pkgbase,
    "arch": arch,
    "build_flags": ["--noconfirm", "--noprogressbar", "--nocolor"],
    "makepkg_conf": makepkg_conf,
    "pacman_conf": pacman_conf,
    "pacman_upgrade_conf": upgrade_conf,
    "mirrorlist": None,
    "pgp_keys": [],
    "keyserver": "hkps://keyserver.ubuntu.com",
    "vcs_sources": (
        [{"source_url": fixture_source, "dir": "fixture", "git_ref": "HEAD"}]
        if "vcs" in options
        else []
    ),
    "cpus": float(options["cpus"]) if "cpus" in options else None,
    "scratch": "/var/lib/aurcache-build",
}
with open(path, "w") as out:
    json.dump(spec, out, indent=2)
EOF
}

# Start a job's container. The first word is `run` to wait for it or `-d` to
# leave it running; what follows the leg and the image are more docker flags.
#   build <run|-d> <leg> <image> [docker flag...]
build() {
    local how=$1 leg=$2 image=$3
    shift 3
    local name
    name=$(container "$leg")
    CONTAINERS+=("$name")
    local command=(docker run --name "$name" "${CONTAINER_FLAGS[@]}"
        -v "$WORK/tools:/aurcache/bin:ro" -v "$WORK/$leg:/work" "$@"
        --entrypoint "$AGENT" "$image" build --work /work --log-file /work/log/build.log)
    if [[ $how == -d ]]; then
        "${command[@]:0:2}" -d "${command[@]:2}" >/dev/null
    else
        timeout "$BUILD_TIMEOUT" "${command[@]}" >"$WORK/$leg.stdout" 2>"$WORK/$leg.stderr"
    fi
}

# Why a leg did not go as expected: the end of what its container wrote.
explain() { # leg
    local file
    for file in "$WORK/$1.stderr" "$WORK/$1.stdout" "$WORK/$1/log/build.log"; do
        [[ -s $file ]] || continue
        echo "    --- the end of ${file#"$WORK/"}"
        tail -n 40 "$file" | sed 's/^/    | /'
        return
    done
}

# A field of a leg's report, as JSON: `report hello ended`.
report() { # leg field
    python3 -c '
import json, sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2:]:
    value = value[key]
print(json.dumps(value, sort_keys=True))' "$WORK/$1/out/report.json" "${@:2}" 2>/dev/null ||
        echo "<none>"
}

# Whether a field of a leg's report is a number above zero.
measured() { # leg field
    [[ $(report "$1" "$2") =~ ^[1-9][0-9]*$ ]] && echo yes || echo no
}

# What a fixture's probe said.
probe() { # leg name
    local line
    line=$(grep -a -m1 "^probe $2=" "$WORK/$1/log/build.log" 2>/dev/null) || line="probe $2=<nothing>"
    echo "${line#*=}"
}

present() { [[ -e $1 || -L $1 ]] && echo present || echo absent; }

packages() { # leg
    find "$WORK/$1/pkg" -mindepth 1 -maxdepth 1 2>/dev/null | sort
}

# What every leg that builds docker/testpkg/hello must leave.
check_hello() { # leg status
    local leg=$1 status=$2 package
    check "$leg: the agent exited 0" 0 "$status"
    check "$leg: makepkg exited 0" '{"code": 0, "how": "exited"}' "$(report "$leg" ended)"
    check "$leg: a peak memory figure" yes "$(measured "$leg" peak_memory_bytes)"
    check "$leg: a disk figure" yes "$(measured "$leg" disk_bytes)"
    check "$leg: no out-of-memory kill" 0 "$(report "$leg" oom_kills)"
    check "$leg: one thing in pkg/" 1 "$(packages "$leg" | wc -l)"
    package=$(packages "$leg" | head -n1)
    check "$leg: and it is the package" yes \
        "$([[ ${package##*/} == aurcache-hello-1.0.0-1-*.pkg.tar.* ]] && echo yes || echo "${package##*/}")"
    check "$leg: the work directory is its owner's again" "$(id -u):$(id -g)" \
        "$(stat -c %u:%g "$WORK/$leg")"
    check "$leg: the package is the owner's, readable by anyone" "$(id -u):$(id -g) 644" \
        "$(stat -c '%u:%g %a' "${package:-/nonexistent}" 2>/dev/null)"
    check "$leg: the report too" "$(id -u):$(id -g) 644" \
        "$(stat -c '%u:%g %a' "$WORK/$leg/out/report.json" 2>/dev/null)"
    # What root would leave there is root's: the owner could not remove it.
    check "$leg: nothing was set aside beside out/" none \
        "$(find "$WORK/$leg" -mindepth 1 -maxdepth 1 -name '.out.old.*' -printf '%f\n' | paste -sd' ' | grep . || echo none)"
    check "$leg: the log file holds what the container wrote" same \
        "$(cmp -s "$WORK/$leg/log/build.log" "$WORK/$leg.stdout" && echo same || echo differs)"
}

echo "==> building the agent, statically linked"
TRIPLE="$(uname -m)-unknown-linux-gnu"
agent=$(RUSTFLAGS="-C target-feature=+crt-static" cargo_executable aurcache-build-agent build \
    --release --target "$TRIPLE" --manifest-path backend/Cargo.toml -p aurcache-build-agent)
mkdir -p "$WORK/tools"
install -m 0755 "$agent" "$WORK/tools/aurcache-build-agent"
if command -v file >/dev/null; then
    file "$WORK/tools/aurcache-build-agent" | sed 's/^[^:]*: /    /'
fi
INTERFACE=$("$WORK/tools/aurcache-build-agent" --interface)
for image in "$IMAGE" "$REPO_SERVER_IMAGE"; do
    docker image inspect "$image" >/dev/null 2>&1 || docker pull -q "$image" >/dev/null
done

echo "==> leg 1/4: a package builds"
prepare hello docker/testpkg/hello hello "$(uname -m)" cpus=1.5
status=0
build run hello "$IMAGE" || status=$?
check_hello hello "$status"
((FAIL)) && explain hello

echo "==> leg 2/4: a hostile package is contained"
mkdir "$WORK/repo"
# The thief, in a pacman repository of its own: the hostile package depends
# on it, so root installs it before the build and its scriptlet runs as
# root.
#
# Beside it, the bait: a far higher version of a package the image has. The
# repository stands for AURCache's own, which the image's upgrade must not
# see, so the bait is not installed. A data-only package the build never
# reads, whose files this would remove.
UPGRADE_BAIT=$(docker run --rm --user 0 --entrypoint /bin/bash "$IMAGE" -c '
    for p in tzdata iana-etc pacman-mirrorlist; do
        if pacman -Q "$p" >/dev/null 2>&1; then echo "$p"; break; fi
    done')
docker run --rm --name "$(container thief)" --user 0 -v "$WORK/repo:/repo" \
    -v "$PWD/docker/testpkg/agent-thief:/thief:ro" \
    -e "BAIT=$UPGRADE_BAIT" --entrypoint /bin/bash "$IMAGE" -c '
        set -e
        work=$(mktemp -d); cd "$work"
        cp /thief/thief.install .INSTALL
        printf "%s\n" "pkgname = aurcache-agent-thief" "pkgbase = aurcache-agent-thief" \
            "pkgver = 1-1" "pkgdesc = fixture" "builddate = 0" "packager = test" "size = 0" \
            "arch = any" >.PKGINFO
        bsdtar -cf - .PKGINFO .INSTALL | zstd -q >/repo/aurcache-agent-thief-1-1-any.pkg.tar.zst
        repo-add -q /repo/thief.db.tar.gz /repo/aurcache-agent-thief-1-1-any.pkg.tar.zst
        # The bait has the architecture the image records for the package:
        # pacman does not upgrade across one.
        if [ -n "$BAIT" ]; then
            arch=$(grep -A1 "%ARCH%" /var/lib/pacman/local/"$BAIT"-*/desc | tail -1)
            mkdir bait; cd bait
            printf "%s\n" "_taken() { mkdir -p /var/tmp/aurcache-upgrade-bait; }" \
                "post_install() { _taken; }" "post_upgrade() { _taken; }" >.INSTALL
            printf "%s\n" "pkgname = $BAIT" "pkgbase = $BAIT" "pkgver = 9999-1" \
                "pkgdesc = upgrade bait" "builddate = 0" "packager = test" "size = 0" \
                "arch = $arch" >.PKGINFO
            bsdtar -cf - .PKGINFO .INSTALL | zstd -q >"/repo/$BAIT-9999-1-$arch.pkg.tar.zst"
            repo-add -q /repo/thief.db.tar.gz "/repo/$BAIT-9999-1-$arch.pkg.tar.zst"
        fi
        chown -R '"$(id -u):$(id -g)"' /repo'
echo "    the upgrade bait: ${UPGRADE_BAIT:-<no package to shadow; that check is skipped>}"
# Served over HTTP, as a job's repositories are: the agent refuses a
# `file://` server.
REPO_SERVER=$(container repo)
CONTAINERS+=("$REPO_SERVER")
docker rm -f "$REPO_SERVER" >/dev/null 2>&1 || true
docker run -d --name "$REPO_SERVER" -v "$WORK/repo:/srv/repo:ro" "$REPO_SERVER_IMAGE" \
    httpd -f -p 8080 -h /srv/repo >/dev/null
REPO_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$REPO_SERVER")
# The repository the fixture's source is.
mkdir -p "$WORK/stage/upstream"
git init -q -b master "$WORK/stage/upstream"
echo "the build agent's test fixture" >"$WORK/stage/upstream/README"
git -C "$WORK/stage/upstream" add README
git -C "$WORK/stage/upstream" -c user.name=test -c user.email=test@aurcache.test commit -qm fixture
git init -q --bare -b master "$WORK/upstream.git"
git -C "$WORK/stage/upstream" push -q "$WORK/upstream.git" master
commit=$(git -C "$WORK/stage/upstream" rev-parse HEAD)
HOSTILE_FLAGS=(-v "$WORK/upstream.git:/srv/aurcache-agent-test/upstream.git:ro")

prepare hostile docker/testpkg/agent-hostile aurcache-agent-hostile "$(uname -m)" vcs=1 \
    "repo=$REPO_IP"
status=0
build run hostile "$IMAGE" "${HOSTILE_FLAGS[@]}" || status=$?
before=$FAIL
check "hostile: the agent exited 0" 0 "$status"
check "hostile: makepkg exited 0" '{"code": 0, "how": "exited"}' "$(report hostile ended)"
# The build ran as the build user, with nothing left of root.
# The build ran as the build user, with nothing left of root, and so did the
# reading of its dependencies.
for at in srcinfo- ""; do
    check "hostile: ${at}uid" 1001 "$(probe hostile "${at}uid")"
    check "hostile: ${at}gid" 1001 "$(probe hostile "${at}gid")"
    check "hostile: ${at}no other group" 1001 "$(probe hostile "${at}groups")"
    for set in capeff capprm capinh capamb; do
        check "hostile: ${at}$set is empty" 0000000000000000 "$(probe hostile "$at$set")"
    done
    check "hostile: ${at}no_new_privs is set" 1 "$(probe hostile "${at}nonewprivs")"
    for attempt in sudo su kill-init term-init read-init-environment write-passwd \
        write-pacman-conf; do
        check "hostile: ${at}$attempt" blocked "$(probe hostile "$at$attempt")"
    done
    # Nothing to ask root for anything: no process of root's but the agent,
    # and no socket but the build user's own.
    check "hostile: ${at}no process of root's" none "$(probe hostile "${at}root-processes")"
    check "hostile: ${at}no socket of anyone else's" 0 "$(probe hostile "${at}foreign-listeners")"
done
# What the reading left running did not live on to see root's install.
check "hostile: what the reading left running was ended" gone "$(probe hostile srcinfo-lingerer)"
# Nothing else it tried worked.
for attempt in write-report rename-out write-work write-log remove-log write-makepkg-conf; do
    check "hostile: $attempt" blocked "$(probe hostile $attempt)"
done
# And what must work did.
check "hostile: pkg/ is the build's to write" worked "$(probe hostile write-pkg)"
check "hostile: the dependency was installed" installed "$(probe hostile dependency)"
check "hostile: the source was cloned" cloned "$(probe hostile source)"
# The image was upgraded, with the job's own repositories alone: the build's
# configuration has the test's repository, the upgrade's has not, and the
# bait in it was left.
check "hostile: the databases were synced and the image upgraded" yes \
    "$(grep -aq 'Synchronizing package databases' "$WORK/hostile/log/build.log" &&
        grep -aq 'Starting full system upgrade' "$WORK/hostile/log/build.log" && echo yes || echo no)"
check "hostile: the build's pacman.conf has the test's repository" 1 \
    "$(probe hostile build-conf-repository)"
check "hostile: the upgrade's has not" 0 "$(probe hostile upgrade-conf-repository)"
if [[ -n $UPGRADE_BAIT ]]; then
    check "hostile: the upgrade left the bait in that repository alone" left \
        "$(probe hostile upgrade-bait)"
else
    echo "    ok   hostile: no package to bait the upgrade with (that check is skipped)"
fi
# A dependency from that repository, whose scriptlet runs as root.
check "hostile: the thief's scriptlet ran as root" yes "$(probe hostile thief-ran)"
check "hostile: orphaned processes are collected" 0 "$(probe hostile zombies)"
# Seen from here: the report is the agent's, and names the source's commit.
check "hostile: the source's commit is reported" "\"$commit\"" \
    "$(report hostile vcs_commits "$FIXTURE_SOURCE")"
check "hostile: out/ holds the report and nothing else" report.json \
    "$(find "$WORK/hostile/out" -mindepth 1 -printf '%P\n' | sort | paste -sd' ')"
check "hostile: nothing was planted beside it" absent "$(present "$WORK/hostile/planted")"
check "hostile: out/ was not moved" absent "$(present "$WORK/hostile/out.moved")"
check "hostile: the package is all pkg/ holds but a directory the build planted" \
    aurcache-agent-hostile-1.0.0-1-any.pkg.tar.zst \
    "$(find "$WORK/hostile/pkg" -mindepth 1 -not -type d -printf '%P\n' | sort | paste -sd' ')"
# Not walked, so no depth can run the agent's stack out: a directory with
# something in it is left to whoever removes the work directory.
check "hostile: the planted directory is left as it was" "not-a-package not-a-package/inside" \
    "$(find "$WORK/hostile/pkg" -mindepth 1 -type d -printf '%P\n' | sort | paste -sd' ')"
((FAIL > before)) && explain hostile

prepare forged docker/testpkg/agent-hostile aurcache-agent-hostile "$(uname -m)" forge=1
status=0
build run forged "$IMAGE" || status=$?
before=$FAIL
check "forged: the agent exited 0" 0 "$status"
check "forged: the build failed installing its dependencies" '"dependencies"' \
    "$(report forged ended step)"
# What the forged `.SRCINFO` named reached root's pacman, after `--`...
check "forged: root was asked for the package file" yes \
    "$(grep -aq '^\[agent\] installing .*aurcache-agent-evil-1-1-any.pkg.tar.zst' \
        "$WORK/forged/log/build.log" && echo yes || echo no)"
# ...as names it has no package for.
check "forged: pacman found no such package" yes \
    "$(grep -aq 'target not found: --overwrite=\*' "$WORK/forged/log/build.log" && echo yes || echo no)"
check "forged: the package file's install script did not run" absent \
    "$(present "$WORK/forged/evil-ran")"
check "forged: makepkg never ran" no \
    "$(grep -aq '^\[agent\] running makepkg' "$WORK/forged/log/build.log" && echo yes || echo no)"
((FAIL > before)) && explain forged

echo "==> leg 3/4: a build that is stopped leaves nothing"
for leg in killed stopped; do
    prepare "$leg" docker/testpkg/agent-hostile aurcache-agent-hostile "$(uname -m)" vcs=1 linger=3600
    build -d "$leg" "$IMAGE" "${HOSTILE_FLAGS[@]}"
done
for leg in killed stopped; do
    name=$(container "$leg")
    deadline=$((SECONDS + BUILD_TIMEOUT))
    until [[ $(probe "$leg" lingering) == 3600 ]]; do
        if [[ $(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null) != true ]] ||
            ((SECONDS > deadline)); then
            break
        fi
        sleep 2
    done
    if [[ $(probe "$leg" lingering) != 3600 ]]; then
        echo "    FAIL $leg: the build never got as far as build()"
        docker logs --tail 40 "$name" 2>&1 | sed 's/^/    | /'
        FAIL=1
        continue
    fi
    # Every process of the container, as this machine numbers them: the agent,
    # makepkg, and what the fixture left behind.
    mapfile -t pids < <(docker top "$name" -o pid 2>/dev/null | tail -n +2)
    check "$leg: the build was running" yes \
        "$( ((${#pids[@]} >= 3)) && echo yes || echo "${#pids[@]} processes")"
    started=$SECONDS
    if [[ $leg == killed ]]; then
        docker kill "$name" >/dev/null
        expected=137
    else
        # Far longer than the agent should need: it ends at once, by itself.
        docker stop -t 300 "$name" >/dev/null
        expected=143
    fi
    code=$(docker wait "$name")
    check "$leg: it ended at once" yes \
        "$( ((SECONDS - started < 60)) && echo yes || echo "$((SECONDS - started))s")"
    check "$leg: the agent's status" "$expected" "$code"
    left=0
    for pid in "${pids[@]}"; do
        [[ -e /proc/$pid ]] && left=$((left + 1))
    done
    check "$leg: no process is left" 0 "$left"
    check "$leg: no report was written" absent "$(present "$WORK/$leg/out/report.json")"
done

echo "==> leg 4/4: an arm64 container, the same agent"
# The arm64 image by digest, so the tag this machine has is left as it is.
arm64_image() {
    local repository=${IMAGE%@*} digest
    [[ ${repository##*/} == *:* ]] && repository=${repository%:*}
    digest=$(docker manifest inspect "$IMAGE" 2>/dev/null | python3 -c '
import json, sys
for manifest in json.load(sys.stdin).get("manifests", []):
    platform = manifest.get("platform", {})
    if platform.get("os") == "linux" and platform.get("architecture") == "arm64":
        print(manifest["digest"])
        break') || return 1
    [[ -n $digest ]] || return 1
    docker pull -q --platform linux/arm64 "$repository@$digest" >/dev/null 2>&1 || return 1
    echo "$repository@$digest"
}
if [[ $(uname -m) != x86_64 ]]; then
    echo "    skipped: this machine is not x86_64"
elif ! grep -qs '^flags:.*F' /proc/sys/fs/binfmt_misc/qemu-aarch64; then
    echo "    skipped: qemu-aarch64 is not registered for containers (binfmt_misc, with the F flag)"
elif ! arm64=$(arm64_image); then
    echo "    skipped: the arm64 image of $IMAGE could not be pulled"
else
    prepare arm64 docker/testpkg/hello hello aarch64
    status=0
    before=$FAIL
    build run arm64 "$arm64" --platform linux/arm64 || status=$?
    check_hello arm64 "$status"
    check "arm64: the package is an aarch64 one" yes \
        "$([[ $(packages arm64) == *-aarch64.pkg.tar.* ]] && echo yes || echo no)"
    ((FAIL > before)) && explain arm64
fi

if ((FAIL)); then
    echo "==> FAILED"
    exit 1
fi
echo "==> PASSED"
