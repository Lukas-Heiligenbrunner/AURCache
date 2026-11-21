#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for build credentials reaching the chroot.
#
# A package whose sources sit behind SSH is the motivating case for worker
# package-affinity (`unreal-engine`), and the credential path has one step no
# unit test can cover: whether the key staged for a job is readable by the
# unprivileged user makepkg runs as inside makechrootpkg. That depends on
# devtools' uid mapping, so it needs a real chroot and a real authenticated
# fetch.
#
# Everything here is ephemeral — a keypair generated per run, a throwaway git
# server that authorises it — so this runs in CI wherever Docker does, with no
# secret committed or configured.
#
# Every build is offered the worker's `default` key unless its package's
# `build_ssh_key` names another. Five phases, in order:
#   1. unauthorised — the worker falls back to its own generated key, which the
#      server does not authorise. The build MUST fail. Without this the whole
#      test could pass for the wrong reason (sources cached, fetch skipped, an
#      agent key picked up).
#   2. authorised — the worker's default key is the one the server accepts.
#      The build must succeed AND the payload's marker must appear in the
#      installed package twice, proving both fetches really happened: the
#      `source=` one on the worker, and the `prepare()` clone inside the
#      chroot, through the agent's socket bound in there.
#   3. pinned host — the same key, with `WORKER_SSH_KNOWN_HOSTS` naming the
#      server's host key. The build must succeed, both fetches checking the
#      server against the staged copy: on the worker, and inside the chroot,
#      where it is bound.
#   4. unknown host — `WORKER_SSH_KNOWN_HOSTS` names another host only. The
#      build MUST fail, refused at host-key verification: the host-side fetch
#      checks the server against the configured list instead of trusting it
#      on first use.
#   5. a named key — the default key is generated, so unauthorised, and the
#      accepted one is held as `gitssh` in `WORKER_GIT_SSH_KEYS`. Built on
#      the default, the fixture MUST fail: it is not offered the `gitssh`
#      key. (That it cannot reach that agent by its path outside the chroot
#      either is `test-sandbox.sh`'s to show.) Its package then names a key
#      the worker lacks, and the build MUST fail with its log saying so.
#      Named `gitssh`, it must succeed.
#
# Where the sandbox, asked as the build user, says it would not restrict UNIX
# sockets (an older kernel's Landlock, a policy that user cannot read, or an
# older aurcache-sandbox), the worker holds no named key, which it says at
# startup: the build naming `gitssh` then MUST fail, saying that key is not
# here.
#
#   ./scripts/test-e2e-ssh.sh [timeout]

BUILD_TIMEOUT="${1:-900}"
export AURCACHE_PORT="${AURCACHE_PORT:-8080}"
export AURCACHE_MIRROR_PORT=$((AURCACHE_PORT + 1))
export AURCACHE_WORKER_PORT=$((AURCACHE_PORT + 3))

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
export AURCACHE_URL="http://localhost:$AURCACHE_PORT/api"
export AURCACHE_TOKEN="${AURCACHE_TOKEN:-}"
export SSH_TEST_MARKER="aurcache-ssh-credential-reached-the-chroot"
# What the fixture's PKGBUILD names its package.
readonly FIXTURE=aurcache-ssh-test

COMPOSE_ARGS=(-f "$PROJECT_DIR/compose/docker-compose.e2e.yaml" -f "$PROJECT_DIR/compose/docker-compose.e2e-ssh.yaml")
E2E_SERVICES="aurcache builder gitssh"

# shellcheck source=scripts/lib/e2e.sh
. "$SCRIPT_DIR/lib/e2e.sh"

KEYDIR="$(mktemp -d -t aurcache-ssh-keys-XXXXXX)"
export SSH_TEST_KEYS="$KEYDIR"
# The keypair is kept when the stack is: without it the surviving containers
# cannot be driven by hand.
E2E_KEEP_HINTS+=("Keypair:    $KEYDIR  (delete when done)")
E2E_CLEANUP_PATHS+=("$KEYDIR")
trap cleanup EXIT

# Bring the stack up and drive one build to a terminal state, leaving what it
# came to in `phase_result`: `success`, `failed`, or why it came to neither.
run_phase() {
    local phase="$1" key="$2" known_hosts="${3-}" named_keys="${4-}" output
    log "=== Phase: $phase ==="
    dc down -v --remove-orphans >/dev/null 2>&1 || true
    fresh_worker_data >/dev/null
    # Must be exported, not just prefixed: `VAR=x dc …` sets it for the shell
    # function but does not reliably reach `docker compose` beneath it, which
    # silently made the "authorised" phase identical to the unauthorised one.
    export WORKER_GIT_SSH_KEY="$key"
    if [ -n "$known_hosts" ]; then
        export WORKER_SSH_KNOWN_HOSTS="$known_hosts"
    else
        unset WORKER_SSH_KNOWN_HOSTS
    fi
    if [ -n "$named_keys" ]; then
        export WORKER_GIT_SSH_KEYS="$named_keys"
    else
        unset WORKER_GIT_SSH_KEYS
    fi
    dc up -d --build >/dev/null

    wait_for_service || { dump_logs_on_failure; exit 1; }
    wait_for_worker || { dump_logs_on_failure; exit 1; }

    log "    adding the fixture package"
    settle pkg_add "git://gitssh/pkg.git" --ref master
}

# Run a build-triggering CLI command under `--wait` and leave what the build
# came to in `phase_result`. The exit status is the outcome. A failure is told
# apart from a timeout or a stalled queue by what `--wait` says, since several
# phases want exactly a failed build and nothing else.
settle() {
    local output
    if output=$("$@" --wait --wait-timeout "$BUILD_TIMEOUT" 2>&1); then
        phase_result=success
    elif [[ $output == *"build failed:"* ]]; then
        phase_result=failed
    else
        phase_result="neither: ${output##*$'\n'}"
    fi
    printf '%s\n' "$output" | sed 's/^/    /'
    CURRENT_BUILD_REF=$(latest_build_ref "$FIXTURE")
}

# Set the fixture's `build_ssh_key` to `$1` and build it again.
rebuild_naming() {
    log "    the fixture's build_ssh_key is now $1"
    aurcache_cli raw PATCH "/package/$FIXTURE/settings/build_ssh_key" \
        --body "{\"value\": \"$1\"}" >/dev/null
    settle aurcache_cli builds retry "$CURRENT_BUILD_REF"
}

# Fail the run unless the log of the last build has `$1` in it.
expect_in_log() {
    if ! aurcache_cli builds output "$CURRENT_BUILD_REF" 2>/dev/null | grep -qF "$1"; then
        log "ERROR: the build log of $CURRENT_BUILD_REF does not say why it had no key"
        log "       (expected: $1)"
        dump_logs_on_failure
        exit 1
    fi
}

E2E_TOOLS+=(ssh-keygen)
preflight
announce_log
build_cli

log "=== Generating a throwaway keypair ==="
ssh-keygen -q -t ed25519 -N '' -C aurcache-ssh-test -f "$KEYDIR/id_ed25519"
# The worker container runs as an unprivileged user whose uid need not match
# the host's, and `mktemp -d` gives 0700 / ssh-keygen gives 0600 — both
# unreadable there. Widen them: this key exists only for the duration of the
# run, and the worker reads it into its ssh-agent at startup, which applies no
# permission check -- the same as a root-owned Docker secret needs.
# The git server's host key, made here so a `known_hosts` can name it before
# the server starts; and one naming another host only.
ssh-keygen -q -t ed25519 -N '' -C gitssh -f "$KEYDIR/host_ed25519"
printf 'gitssh %s\n' "$(cut -d' ' -f1,2 "$KEYDIR/host_ed25519.pub")" >"$KEYDIR/known_hosts"
ssh-keygen -q -t ed25519 -N '' -C elsewhere -f "$KEYDIR/other_host_ed25519"
printf 'elsewhere.invalid %s\n' "$(cut -d' ' -f1,2 "$KEYDIR/other_host_ed25519.pub")" \
    >"$KEYDIR/known_hosts.other"
chmod 755 "$KEYDIR"
chmod 644 "$KEYDIR/id_ed25519" "$KEYDIR/id_ed25519.pub" "$KEYDIR/known_hosts" \
    "$KEYDIR/known_hosts.other" "$KEYDIR/host_ed25519.pub" "$KEYDIR/host_ed25519"

# ---- Phase 1: the fixture must genuinely require the credential ------------
run_phase "unauthorised (expect failure)" ""
if [ "$phase_result" != "failed" ]; then
    log "ERROR: build was '$phase_result' without an authorised key; the test"
    log "       would prove nothing — the source may be cached or the fetch skipped."
    dump_logs_on_failure
    exit 1
fi
log "    build failed as required"

# ---- Phase 2: with the authorised key it must build ------------------------
run_phase "authorised (expect success)" "/keys/id_ed25519"
if [ "$phase_result" != "success" ]; then
    log "ERROR: build was '$phase_result' with the authorised key"
    dump_logs_on_failure
    exit 1
fi
log "    build succeeded"

# Guard against the phase collapsing again: if the worker fell back to a
# self-generated key, the phases would be identical and a pass would mean
# nothing.
if dc logs builder 2>&1 | grep -q "Build SSH key:"; then
    log "ERROR: the worker generated its own key; WORKER_GIT_SSH_KEY did not"
    log "       reach the container, so this phase proves nothing."
    exit 1
fi

# ---- The marker proves the SSH fetch actually happened ---------------------
log "=== Verifying the payload reached the package ==="
found="$(docker run --rm -i --network "$(e2e_network)" -e FIXTURE="$FIXTURE" \
    archlinux:latest bash -e 2>/dev/null <<'EOF' || true
printf '[repo]\nSigLevel = Never\nServer = http://aurcache:8081/$arch\n' >> /etc/pacman.conf
pacman -Sy --noconfirm >/dev/null 2>&1
pacman -S --noconfirm "$FIXTURE" >/dev/null 2>&1
cat "/usr/share/$FIXTURE/marker.txt" "/usr/share/$FIXTURE/cloned-marker.txt"
EOF
)"

if [ "$found" != "$SSH_TEST_MARKER"$'\n'"$SSH_TEST_MARKER" ]; then
    log "ERROR: a marker is missing from the built package (got: '${found:-<nothing>}')"
    exit 1
fi

# ---- Phase 3: a configured known_hosts naming the server must build --------
run_phase "known_hosts names the server (expect success)" "/keys/id_ed25519" "/keys/known_hosts"
if [ "$phase_result" != "success" ]; then
    log "ERROR: build was '$phase_result' with the server's host key in WORKER_SSH_KNOWN_HOSTS"
    dump_logs_on_failure
    exit 1
fi
log "    build succeeded against the pinned host key"

# ---- Phase 4: a host the configured known_hosts does not name is refused ----
run_phase "known_hosts names another host (expect failure)" "/keys/id_ed25519" "/keys/known_hosts.other"
if [ "$phase_result" != "failed" ]; then
    log "ERROR: build was '$phase_result' against a host WORKER_SSH_KNOWN_HOSTS does not name"
    dump_logs_on_failure
    exit 1
fi
if ! aurcache_cli builds output "$CURRENT_BUILD_REF" 2>/dev/null | grep -qF "Host key verification failed"; then
    log "ERROR: the build of $CURRENT_BUILD_REF failed, but not at host-key verification"
    dump_logs_on_failure
    exit 1
fi
log "    build refused the unknown host at host-key verification"

# ---- Phase 5: a named key goes to the package that names it, and only it ---
run_phase "named key, built on the default (expect failure)" "" "" "gitssh:/keys/id_ed25519"
if [ "$phase_result" != "failed" ]; then
    log "ERROR: build was '$phase_result' on the default key, with the authorised key"
    log "       held only as gitssh"
    dump_logs_on_failure
    exit 1
fi
log "    build on the default key failed: it was not offered the gitssh key"
log "    (that it cannot reach that agent by its path is test-sandbox.sh's to show)"

# Named keys need the sandbox to keep builds from the agents of the keys they
# are not offered, outside their chroot; where it cannot, the worker holds none.
if dc logs builder 2>&1 | grep -q "outside their chroot as well (Landlock)"; then
    named_held=yes
    log "    named keys held: the sandbox keeps builds from the other agents (Landlock)"
elif dc logs builder 2>&1 | grep -q "WORKER_GIT_SSH_KEYS names SSH keys, but builds could not be kept"; then
    named_held=no
    log "    named keys not held here; the build naming gitssh must fail"
else
    log "ERROR: the worker did not say whether it holds its named keys"
    dump_logs_on_failure
    exit 1
fi

rebuild_naming missing
if [ "$phase_result" != "failed" ]; then
    log "ERROR: build was '$phase_result' naming a key the worker does not hold"
    dump_logs_on_failure
    exit 1
fi
expect_in_log 'this worker holds no SSH key named "missing"'
log "    build naming a key the worker lacks failed, and its log says why"

rebuild_naming gitssh
if [ "$named_held" = yes ]; then
    if [ "$phase_result" != "success" ]; then
        log "ERROR: build was '$phase_result' naming the authorised key, gitssh"
        dump_logs_on_failure
        exit 1
    fi
    log "    build naming gitssh succeeded"
else
    if [ "$phase_result" != "failed" ]; then
        log "ERROR: build was '$phase_result' naming gitssh on a worker that holds no named key"
        dump_logs_on_failure
        exit 1
    fi
    expect_in_log 'this worker holds no SSH key named "gitssh"'
    log "    build naming gitssh failed, as the worker holds no named key here"
fi

log "=== SSH credential test complete ==="
log "    markers: ${found//$'\n'/, }"
