#!/usr/bin/env bash
# The build agent, for real: `aurcache-build-agent build` in the builder image,
# under exactly what a build pod's security context gives its container
# (user 0, no-new-privileges, every capability dropped but seven).
#
# It builds the agent as it ships (statically linked) and runs four legs:
#
#   1. docker/testpkg/hello builds: a report, measurements, and one package
#      handed back to the work directory's owner;
#   2. docker/testpkg/agent-hostile builds, and from inside build() tries to
#      become root, find the worker's SSH key, write the report and get root's
#      pacman helper to do anything but install named dependencies. Both
#      polarities: its dependency is only there if the helper installed it.
#      It fetches a source over git+ssh from a throwaway server
#      (docker/git-ssh.Dockerfile), which works only through the SSH link a
#      stand-in for the worker serves from here, and it depends on
#      docker/testpkg/agent-thief, whose install scriptlet, as root, hunts for
#      the key and leaves a process behind that keeps hunting: it finds
#      nothing;
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
AGENT=/aurcache/bin/aurcache-build-agent
# aurcache_build_agent::layout::CAPABILITIES
CAPABILITIES=(CHOWN DAC_OVERRIDE FOWNER FSETID SETUID SETGID KILL)
# A build pod's security context, as Docker flags.
POD_FLAGS=(--user 0 --security-opt no-new-privileges --cap-drop=ALL "${CAPABILITIES[@]/#/--cap-add=}")
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
    + "".join(f"\n[{repo}]\nInclude = /etc/pacman.d/mirrorlist\n" for repo in repos)
)
# The first two are the agent's to decide, and must not win.
makepkg_conf = "PKGDEST=/tmp/not-here\nMAKEFLAGS=-j1\n"
if "vcs" in options:
    # The fixture's repository belongs to whoever runs this script, and git
    # will not serve a clone from someone else's without being told to.
    makepkg_conf += (
        "export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'\n"
    )
if "gitssh" in options:
    makepkg_conf += f"AURCACHE_TEST_GITSSH={options['gitssh']}\n"
    pacman_conf += f"\n[thief]\nServer = http://{options['gitssh']}:8080\n"
if "linger" in options:
    makepkg_conf += f"AURCACHE_TEST_LINGER={options['linger']}\n"
makepkg_conf += "PACKAGER='AURCache <aurcache@localhost>'\nOPTIONS+=(!debug)\n"

spec = {
    "interface": int(options["interface"]),
    "pkgbase": pkgbase,
    "build_flags": ["--noconfirm", "--noprogressbar", "--nocolor"],
    "makepkg_conf": makepkg_conf,
    "pacman_conf": pacman_conf,
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
    "ssh": (
        {"known_hosts": open(options["known_hosts"]).read()}
        if "known_hosts" in options
        else None
    ),
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
    local command=(docker run --name "$name" "${POD_FLAGS[@]}"
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
echo "==> building the worker's end of the SSH link"
LINK_SERVE=$(cargo_executable ssh-link-serve build --manifest-path backend/Cargo.toml \
    -p aurcache-worker-container --example ssh-link-serve)
docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull -q "$IMAGE" >/dev/null

echo "==> leg 1/4: a package builds"
prepare hello docker/testpkg/hello hello "$(uname -m)" cpus=1.5
status=0
build run hello "$IMAGE" || status=$?
check_hello hello "$status"
((FAIL)) && explain hello

echo "==> leg 2/4: a hostile package is contained"
command -v ssh-keygen >/dev/null || {
    echo "error: ssh-keygen is needed to make the test credential" >&2
    exit 2
}
# The worker's SSH key, which stays out here, and the server it opens.
mkdir "$WORK/ssh" "$WORK/repo"
ssh-keygen -q -t ed25519 -N '' -C aurcache-agent-test -f "$WORK/ssh/id_ed25519"
# The thief, in a pacman repository of its own.
docker run --rm --name "$(container thief)" --user 0 -v "$WORK/repo:/repo" \
    -v "$PWD/docker/testpkg/agent-thief:/thief:ro" --entrypoint /bin/bash "$IMAGE" -c '
        set -e
        cd "$(mktemp -d)"
        cp /thief/thief.install .INSTALL
        printf "%s\n" "pkgname = aurcache-agent-thief" "pkgbase = aurcache-agent-thief" \
            "pkgver = 1-1" "pkgdesc = fixture" "builddate = 0" "packager = test" "size = 0" \
            "arch = any" >.PKGINFO
        bsdtar -cf - .PKGINFO .INSTALL | zstd -q >/repo/aurcache-agent-thief-1-1-any.pkg.tar.zst
        repo-add -q /repo/thief.db.tar.gz /repo/aurcache-agent-thief-1-1-any.pkg.tar.zst
        chown -R '"$(id -u):$(id -g)"' /repo'
docker build -q -t "$PREFIX-agent-gitssh" -f docker/git-ssh.Dockerfile . >/dev/null
GITSSH=$(container gitssh)
CONTAINERS+=("$GITSSH")
docker rm -f "$GITSSH" >/dev/null 2>&1 || true
docker run -d --name "$GITSSH" -v "$WORK/ssh/id_ed25519.pub:/keys/id_ed25519.pub:ro" \
    -v "$WORK/repo:/srv/repo:ro" "$PREFIX-agent-gitssh" >/dev/null
GITSSH_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$GITSSH")
for _ in $(seq 60); do
    ssh-keyscan -t ed25519 "$GITSSH_IP" >"$WORK/ssh/known_hosts" 2>/dev/null && [[ -s $WORK/ssh/known_hosts ]] && break
    sleep 1
done
[[ -s $WORK/ssh/known_hosts ]] || {
    echo "error: the git server at $GITSSH_IP never answered" >&2
    exit 1
}
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
    "known_hosts=$WORK/ssh/known_hosts" "gitssh=$GITSSH_IP"
build -d hostile "$IMAGE" "${HOSTILE_FLAGS[@]}"
# The worker's side: one exec of ssh-link, as the container starts.
timeout "$BUILD_TIMEOUT" "$LINK_SERVE" "$WORK/ssh/id_ed25519" -- docker exec -i "$(container hostile)" \
    "$AGENT" ssh-link --socket /var/lib/aurcache-build/ssh-link/link.sock \
    >"$WORK/hostile.link" 2>&1 || true
status=$(timeout "$BUILD_TIMEOUT" docker wait "$(container hostile)" || echo timeout)
docker logs "$(container hostile)" >"$WORK/hostile.stdout" 2>"$WORK/hostile.stderr" || true
before=$FAIL
check "hostile: the agent exited 0" 0 "$status"
check "hostile: makepkg exited 0" '{"code": 0, "how": "exited"}' "$(report hostile ended)"
# The build ran as the build user, with nothing left of root.
check "hostile: uid" 1001 "$(probe hostile uid)"
check "hostile: gid" 1001 "$(probe hostile gid)"
check "hostile: no other group" 1001 "$(probe hostile groups)"
for set in capeff capprm capinh capamb; do
    check "hostile: $set is empty" 0000000000000000 "$(probe hostile $set)"
done
check "hostile: no_new_privs is set" 1 "$(probe hostile nonewprivs)"
# Nothing it tried worked.
for attempt in sudo su kill-init term-init read-init-environment write-passwd write-pacman-conf \
    write-report rename-out write-work write-log remove-log \
    write-makepkg-conf remove-helper-socket; do
    check "hostile: $attempt" blocked "$(probe hostile $attempt)"
done
for request in upgrade upgrade-noconfirm config path relative-path repo remove explicit refresh \
    root dbpath overwrite hookdir shell not-pacman nothing; do
    check "hostile: helper, $request" refused "$(probe hostile "helper-$request")"
done
check "hostile: no install script ran as root" not-run "$(probe hostile install-script)"
# The thief's hunt sends the helper what it sends every socket, which is
# refused as no request at all; those are not counted.
check "hostile: each refusal is in the build log" 16 \
    "$(grep -a '^\[agent\] refused a request to run pacman as root' "$WORK/hostile/log/build.log" |
        grep -a -c -v 'is not a list of words')"
# And what must work did.
check "hostile: pkg/ is the build's to write" worked "$(probe hostile write-pkg)"
check "hostile: the helper runs what makepkg sends" ran:0 "$(probe hostile helper-asdeps)"
check "hostile: the dependency was installed" installed "$(probe hostile dependency)"
check "hostile: the source was cloned" cloned "$(probe hostile source)"
# The worker's key: used while the sources were downloaded, and nowhere in
# reach afterwards, of the build or of a dependency's root scriptlet.
check "hostile: the download listed the key through the link" listed \
    "$(probe hostile download-agent-key)"
check "hostile: the git+ssh source was fetched" fetched "$(probe hostile payload)"
check "hostile: the link closed, having signed" yes \
    "$(grep -Eq 'the SSH link closed: [1-9][0-9]* signature' "$WORK/hostile.link" && echo yes ||
        cat "$WORK/hostile.link")"
check "hostile: no key file in the container" 0 "$(probe hostile key-files)"
check "hostile: the agent socket is gone in build()" absent "$(probe hostile ssh-socket)"
check "hostile: the link socket is gone in build()" absent "$(probe hostile link-socket)"
check "hostile: no agent lists the key in build()" missing "$(probe hostile agent-key)"
check "hostile: the thief's scriptlet ran as root" yes "$(probe hostile thief-ran)"
check "hostile: the thief found no key file" 0 "$(probe hostile thief-keys)"
check "hostile: no socket gave the thief the key" 0 "$(probe hostile thief-agents)"
check "hostile: nor later, to what it left running" 0 "$(probe hostile thief-later)"
check "hostile: which kept looking" yes \
    "$( (($(probe hostile thief-looked) > 0)) && echo yes || echo no)"
check "hostile: orphaned processes are collected" 0 "$(probe hostile zombies)"
# Seen from here: the report is the agent's, and names the source's commit.
check "hostile: the source's commit is reported" "\"$commit\"" \
    "$(report hostile vcs_commits "$FIXTURE_SOURCE")"
check "hostile: out/ holds the report and nothing else" report.json \
    "$(find "$WORK/hostile/out" -mindepth 1 -printf '%P\n' | sort | paste -sd' ')"
check "hostile: nothing was planted beside it" absent "$(present "$WORK/hostile/planted")"
check "hostile: out/ was not moved" absent "$(present "$WORK/hostile/out.moved")"
check "hostile: pkg/ holds the package and nothing the build planted" \
    aurcache-agent-hostile-1.0.0-1-any.pkg.tar.zst \
    "$(find "$WORK/hostile/pkg" -mindepth 1 -printf '%P\n' | sort | paste -sd' ')"
((FAIL > before)) && explain hostile

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
