#!/bin/bash
set -e
# AURCache worker entrypoint.
#
# Bundled auto-approval works by the worker dropping its CSR into a shared
# `enroll` volume that the server reads. The worker runs as the unprivileged
# `builder` user, but a Docker named volume is initialised root-owned by
# whichever container mounts it first (often the server, read-only). Use the
# build user's passwordless sudo to take ownership of just that directory so the
# CSR can be written. Best-effort: if there is no enroll dir (token/preapproved
# enrollment, or a remote worker), this is a no-op.
if [ -n "${AURCACHE_ENROLLMENT_DIR:-}" ]; then
    sudo mkdir -p "${AURCACHE_ENROLLMENT_DIR}" 2>/dev/null || true
    sudo chown "$(id -u):$(id -g)" "${AURCACHE_ENROLLMENT_DIR}" 2>/dev/null || true
fi

# Hand this container's cgroup subtree to the worker user.
#
# The worker gives each build a cgroup of its own so `memory.peak` reports one
# build rather than the whole container. Creating one means writing under
# /sys/fs/cgroup, and while a privileged container mounts that read-write, it is
# owned by root and the worker deliberately runs as `aurcache`.
#
# The native unit gets this for free: systemd's `Delegate=yes` chowns the unit's
# subtree to its `User=`. A container has no systemd to do it, so do the same
# thing here with the sudo the worker already has. Chowning the namespace root
# reaches only this container's own subtree -- in a private cgroup namespace
# that directory *is* the container's cgroup.
#
# Best-effort: an unprivileged container has /sys/fs/cgroup read-only, and the
# worker then reports no memory figure and builds exactly as before.
if [ -d /sys/fs/cgroup ]; then
    for f in /sys/fs/cgroup \
             /sys/fs/cgroup/cgroup.procs \
             /sys/fs/cgroup/cgroup.subtree_control; do
        sudo chown "$(id -u):$(id -g)" "$f" 2>/dev/null || true
    done
fi

exec /usr/bin/aurcache-worker "$@"
