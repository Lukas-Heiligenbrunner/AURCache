#!/bin/bash
# AURCache hybrid (compatibility) entrypoint: run the server and one embedded
# build worker in a single container.
#
# **Deprecated.** This exists so a pre-remote-worker compose file keeps building
# packages after an upgrade with no edits. The split `aurcache-server` +
# `aurcache-worker` images are the supported setup.
#
# Everything temporary about the compatibility path lives in this script — the
# shared secret, the build-mode inference, the supervision. The worker binary
# does not know it is embedded, so retiring this image retires the whole
# mechanism with it.
#
# The embedded worker is the legacy container builder, the one closest to what
# these deployments ran before: a container per package, spawned over a Docker
# socket. That keeps the old requirements the old requirements -- no new
# kernel, no loop devices, foreign architectures through binfmt as before.
set -uo pipefail

log() { printf '[hybrid] %s\n' "$*" >&2; }

# ---------------------------------------------------------------------------
# Trust between the two processes.
#
# The embedded worker enrolls over the ordinary mTLS path — real CSR, real
# certificate, real worker row — and is auto-approved by a shared secret that
# never leaves this container. Regenerating it per boot is harmless: the token
# only ever approves a *pending* worker, so it cannot re-approve one that was
# revoked, nor disturb one already approved.
# ---------------------------------------------------------------------------
export AURCACHE_ENROLLMENT_TOKEN="${AURCACHE_ENROLLMENT_TOKEN:-$(head -c 32 /dev/urandom | base64 | tr -d '\n')}"
export AURCACHE_URL="${AURCACHE_URL:-https://localhost:8083}"
# The worker dials the server as `localhost`, so the listener's certificate has
# to be valid for it.
export AURCACHE_TLS_SANS="${AURCACHE_TLS_SANS:-localhost}"
# Auto-approval here is by token, not by shared volume; make sure a stale
# enrollment directory cannot also grant it.
unset AURCACHE_ENROLLMENT_DIR

# Name the embedded worker for what it is.
#
# Its default name is the hostname, which inside Docker is the container id --
# so every recreate produced a row called something like `b3f4a648533a`, and the
# Workers page read as a crowd of strangers rather than one worker that had come
# back. It is not a machine anyone chose to add; it is the one that ships in the
# box, and it should say so. Still overridable for a host running more than one.
export WORKER_NAME="${WORKER_NAME:-bundled}"

# The pre-worker setting, under its new name. It was the server's cap on builds
# at once; it is now the worker's own concurrency, which is the same thing
# here, where the server has exactly one worker.
if [ -n "${MAX_CONCURRENT_BUILDS:-}" ] && [ -z "${WORKER_CONCURRENCY:-}" ]; then
    export WORKER_CONCURRENCY_DEFAULT="${WORKER_CONCURRENCY_DEFAULT:-$MAX_CONCURRENT_BUILDS}"
fi

# Foreign architectures this host can run, as the old builder could: it pulled
# each build's image for the package's platform and let binfmt run it. Read
# from the handlers registered on the host kernel, which is what decides
# whether such a build can work at all.
if [ -z "${WORKER_EMULATED_ARCHES:-}" ]; then
    native=$(uname -m)
    emulated=()
    for pair in x86_64:qemu-x86_64 aarch64:qemu-aarch64 armv7h:qemu-arm; do
        arch=${pair%%:*} handler=${pair#*:}
        [ "$arch" = "$native" ] && continue
        [ "$arch" = armv7h ] && [ "$native" = armv7l ] && continue
        if grep -qx enabled "/proc/sys/fs/binfmt_misc/$handler" 2>/dev/null; then
            emulated+=("$arch")
        fi
    done
    if [ ${#emulated[@]} -gt 0 ]; then
        WORKER_EMULATED_ARCHES=$(IFS=,; echo "${emulated[*]}")
        export WORKER_EMULATED_ARCHES
        log "emulated architectures available through binfmt: $WORKER_EMULATED_ARCHES"
    fi
fi

# ---------------------------------------------------------------------------
# Which Docker API the builder talks to.
#
# `BUILD_ARTIFACT_DIR` is not a hint — in the pre-worker code it *was* the
# definition of host build mode (`get_build_mode()` branched on exactly this).
# So a deployment that sets it mounted the host's Docker socket, and the builder
# uses that. Everything else was DinD mode: a privileged container running its
# own Podman behind a Docker-compatible socket, which is what happens here too.
# ---------------------------------------------------------------------------
declare -a PIDS=()
WORKER=1

if [ -n "${BUILD_ARTIFACT_DIR:-}" ]; then
    log "host build mode: building against the mounted Docker socket"
else
    log "DinD build mode: starting Podman"
    podman system service --time=0 unix:///var/run/docker.sock &
    PIDS+=($!)
    for _ in $(seq 50); do
        [ -S /var/run/docker.sock ] && podman info >/dev/null 2>&1 && break
        sleep 0.2
    done
    if ! podman info >/dev/null 2>&1; then
        log "ERROR: Podman cannot run in this container, so no packages can be"
        log "       built here."
        log ""
        log "       Add 'privileged: true' to this service in your compose file"
        log "       and recreate it, as the single-container setup always needed."
        log "       If your setup instead mounts the Docker socket, set"
        log "       BUILD_ARTIFACT_DIR as it was before upgrading."
        log ""
        log "       Starting the server alone; the web UI will be reachable and"
        log "       will report that no worker is available."
        WORKER=""
    fi
    # Podman runs in this container, so its "host" network is this container's
    # own: builds reach the repository at localhost, as they do through
    # `container:<id>` against a host daemon -- which is the builder's default,
    # and names a container this Podman has never heard of.
    export AURCACHE_BUILDER_NETWORK="${AURCACHE_BUILDER_NETWORK:-host}"
    # And the build directory is one path, seen the same way from both sides.
    export BUILD_ARTIFACT_DIR=/app/builds
    export BUILD_ARTIFACT_DIR_LOCAL=/app/builds
    mkdir -p /app/builds
fi

# ---------------------------------------------------------------------------
# Supervision. Several processes, one PID 1: if any exits, bring the container
# down so the restart policy applies. A container that stays up while silently
# building nothing is the failure this whole image exists to prevent.
# ---------------------------------------------------------------------------
shutdown() {
    trap - TERM INT
    for pid in "${PIDS[@]}"; do kill -TERM "$pid" 2>/dev/null; done
    wait
}
trap shutdown TERM INT

log "starting AURCache server"
# Close the server's state to other users; see private-state.sh.
/usr/local/bin/aurcache-private-state
/usr/bin/aurcache &
PIDS+=($!)

if [ -n "$WORKER" ]; then
    log "starting embedded worker: legacy container builder"
    log "NOTE: the hybrid image is deprecated. Migrate to the separate"
    log "      aurcache-server and aurcache-worker images when convenient."
    # Root, as the pre-worker server was: it drives a root-owned socket, and
    # runs no build code itself -- each build runs in a container of its own.
    /usr/bin/aurcache-worker-docker run &
    PIDS+=($!)
fi

wait -n
status=$?
log "a supervised process exited (status $status); shutting down"
shutdown
exit "$status"
