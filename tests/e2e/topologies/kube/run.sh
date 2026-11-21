#!/usr/bin/env bash
# End-to-end test for the Kubernetes worker, in a throwaway kind cluster.
#
# Builds the server and aurcache-worker-kube images from this tree, loads them
# into a new kind cluster, deploys the server from kubernetes/server/ and the
# worker from kubernetes/kube-worker/ as they ship (the kustomization beside
# this script adds what a test needs), and drives the server with the CLI:
#
#   1. the worker enrolls through the `aurcache-enrollment` Secret, shows as
#      `kubernetes`, and takes the build agent from the image it runs itself;
#      it has loaded its SSH key into an ssh-agent, and its pod would be
#      admitted to a namespace that enforces `restricted`;
#   2. the worker finds the builds' NetworkPolicy enforced; and asked by hand,
#      a pod labelled like a build cannot open the server's worker port but
#      reaches its repository, where a pod outside the policy reaches both;
#   3. with a ResourceQuota that admits no pod, the worker claims nothing;
#      once it is gone, <package> builds, with its output in the build log, a
#      peak memory figure and disk use;
#   4. a package whose dependency only AURCache's repository has builds
#      against it, and both install with pacman from a pod;
#   4b. a package fetches a source over git+ssh with the dispatcher's SSH
#      key (never mounted into a build pod) and reaches the server again
#      from build(); it and a dependency of it from AURCache's repository,
#      whose install script runs as root in the pod, look for the key and
#      ask the agent for a signature that is not a login: neither finds a
#      key, where each finds the canary it planted, and both get the
#      signature, as ssh-agent signs what it is asked to;
#   5. a build is cancelled, and its pod is gone within a minute;
#   6. the worker is killed while a build runs; the new process deletes the
#      build pod, and the server ends the build and queues a new one;
#   7. the worker's pod is deleted while a build runs; it stops within its
#      grace period, having deleted the build pod and handed the build back,
#      and the server queues a new one;
#   8. with a RuntimeClass whose handler the node lacks, a build pod never
#      starts: the build is handed back as the worker's once the schedule
#      timeout has passed, and builds again once the worker is put right;
#   9. a package of 2 GiB (tests/fixtures/packages/big) builds and is published; the
#      relay's throughput and the worker container's peak memory are printed,
#      and the run fails if that memory reached 64 MiB;
#  10. build pods in user namespaces, where the cluster's nodes run one;
#      skipped, saying why, where they do not;
#  11. without the NetworkPolicy, the worker says it is not enforced, and the
#      server hears it.
#
#   tests/e2e/topologies/kube/run.sh [package] [timeout]
#
# package defaults to `hello` (from the AUR), timeout to 900 seconds a build.
#
# Needs kind, kubectl, docker, jq, curl, cargo, python3, ssh-keygen and pkill,
# and the packaging submodules (`git submodule update --init`) for the image
# builds.
#
# Environment:
#   AURCACHE_TEST_PREFIX  prefix of the cluster, the images and the scratch
#                         directory (default aurcache-test)
#   KIND, KUBECTL         the binaries (default: from PATH)
#   E2E_SCRATCH           where the cluster keeps its node's data: containerd's
#                         images, the pods' volumes and the server's, and the
#                         builder image on its way in. ~10 GB with the 2 GiB
#                         package; default $TMPDIR/<prefix>-kube-e2e. `kind
#                         load` stages the other images in $TMPDIR
#   E2E_BUILDER_IMAGE     the builder image builds run in (default
#                         ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest);
#                         loaded from the local Docker when it is there, pulled
#                         by the node otherwise
#   E2E_SKIP_BUILD=1      use the images already built under the prefix
#   E2E_BIG=0             skip the 2 GiB package
#   E2E_USER_NAMESPACES   auto (default): run step 10 where the node can;
#                         1: fail where it cannot; 0: skip it
#   E2E_MAX_WORKER_MEMORY the worker's peak memory the run fails at, in bytes
#                         (default 64 MiB; it has measured 12 to 18 MiB, and
#                         the container's own limit is 256 MiB)
#   KEEP=1                leave everything up, pass or fail, to look into it
#   REUSE=1               run against the cluster a kept run left, without
#                         building or deploying anything; it is kept again
#                         unless CLEANUP=1. A package the cluster has built
#                         before is built again by retrying its latest build
#   E2E_LEGS              which legs to run, space-separated, of: enrollment
#                         policy quota repository ssh cancel restart stop
#                         stuck big userns open-policy (default: all, in that
#                         order)
#   CLEANUP               as for the other e2e suites: 1 always tears down, 0
#                         never; unset, a failed run is left up
set -euo pipefail

PACKAGE="${1:-hello}"
BUILD_TIMEOUT="${2:-900}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
REPO_ROOT="$PROJECT_DIR"
# The kustomization the cluster is deployed from, beside this script.
FIXTURE="$SCRIPT_DIR"

PREFIX="${AURCACHE_TEST_PREFIX:-aurcache-test}"
CLUSTER="$PREFIX-kube-e2e"
NODE="$CLUSTER-control-plane"
KIND="${KIND:-kind}"
KUBECTL="${KUBECTL:-kubectl}"
# The node image kind v0.33.0 was released with, by digest.
NODE_IMAGE="kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5"
SCRATCH="${E2E_SCRATCH:-${TMPDIR:-/tmp}/$CLUSTER}"
REGISTRY="localhost/$PREFIX"
TAG=kube-e2e
SERVER_IMAGE="$REGISTRY/aurcache-server:$TAG"
WORKER_IMAGE="$REGISTRY/aurcache-worker-kube:$TAG"
GITSSH_IMAGE="$REGISTRY/aurcache-gitssh:$TAG"
BUILDER_IMAGE="${E2E_BUILDER_IMAGE:-ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest}"
MAX_WORKER_MEMORY="${E2E_MAX_WORKER_MEMORY:-$((64 * 1024 * 1024))}"
# How long a Deployment gets to roll out: a new pod and its image, on a
# machine that may be doing other things.
ROLLOUT=600s
GIT_URL="git://kube-e2e-git.kube-e2e.svc"
[ "${KEEP:-0}" = 1 ] && CLEANUP=0
[ "${REUSE:-0}" = 1 ] && CLEANUP=${CLEANUP:-0}

export KUBECONFIG="$SCRATCH/kubeconfig"
export AURCACHE_TOKEN="${AURCACHE_TOKEN:-}"

# shellcheck source=scripts/lib/common.sh
. "$PROJECT_DIR/scripts/lib/common.sh"
# The compose-based harness's helpers: logging, `pkg_add` and its retry, the
# build lookups. What it does with compose is replaced below.
COMPOSE_ARGS=()
E2E_SERVICES=""
# shellcheck source=scripts/lib/e2e.sh
. "$PROJECT_DIR/scripts/lib/e2e.sh"

FORWARD_PID=
ADD_PID=
FOLLOW_PID=
CLUSTER_CREATED=0
RESULTS=()

k() { "$KUBECTL" "$@"; }

aurcache_cli() {
    ensure_forward
    "$CLI_BIN" "$@"
}

# What a leg found, for the summary at the end.
result() {
    RESULTS+=("$*")
    log "    => $*"
}

fail() {
    log "ERROR: $*"
    dump_logs_on_failure
    exit 1
}

# Run `$@` every two seconds until it succeeds, for at most `$1` seconds.
wait_for() {
    local timeout=$1 what=$2
    shift 2
    local deadline=$((SECONDS + timeout))
    until "$@"; do
        if ((SECONDS >= deadline)); then
            log "ERROR: gave up after ${timeout}s waiting for $what"
            return 1
        fi
        sleep 2
    done
}

now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# =============================================================================
# The worker's log
# =============================================================================

dispatcher_pod() {
    k -n aurcache get pods -l app.kubernetes.io/name=aurcache-kube-worker \
        -o jsonpath='{.items[0].metadata.name}'
}

# The running worker container's log, from `$1` (RFC 3339) when given.
dispatcher_log() {
    local from=()
    [ -n "${1:-}" ] && from=(--since-time="$1")
    k -n aurcache logs deploy/aurcache-kube-worker -c worker "${from[@]}" 2>/dev/null \
        | sed 's/\x1b\[[0-9;]*m//g'
}

# Whether the worker logged a line matching `$1` since `$2`.
dispatcher_said() { dispatcher_log "${2:-}" | grep -qE -- "$1"; }

# Whether the worker logged no line matching `$1` since `$2`. A log that
# cannot be read fails the run: it says nothing either way.
dispatcher_silent() {
    local text
    text=$(dispatcher_log "${2:-}") || fail "the worker's log could not be read"
    ! grep -qE -- "$1" <<<"$text"
}

# Wait for the worker to log `$1`, since `$2`, for `$3` seconds.
expect_said() {
    local pattern=$1 since=${2:-} timeout=${3:-120}
    if ! wait_for "$timeout" "the worker to log /$pattern/" dispatcher_said "$pattern" "$since"; then
        fail "the worker never logged /$pattern/"
    fi
    dispatcher_log "$since" | grep -E -- "$pattern" | tail -n 1 | sed 's/^/    | /'
}

dispatcher_restarts() {
    k -n aurcache get pod "$(dispatcher_pod)" \
        -o jsonpath='{.status.containerStatuses[0].restartCount}'
}

dispatcher_running_after() {
    local before=$1 state
    [ "$(dispatcher_restarts)" -gt "$before" ] || return 1
    state=$(k -n aurcache get pod "$(dispatcher_pod)" \
        -o jsonpath='{.status.containerStatuses[0].state.running.startedAt}')
    [ -n "$state" ]
}

# Kill the worker's process outright, as a crash or an OOM kill would, and
# wait for the kubelet to start it again in the same pod.
crash_dispatcher() {
    local pod cid before
    pod=$(dispatcher_pod)
    before=$(dispatcher_restarts)
    cid=$(k -n aurcache get pod "$pod" -o jsonpath='{.status.containerStatuses[0].containerID}')
    # The kill is at once (`stop --timeout 0`); crictl itself gives the
    # runtime two seconds to answer unless told otherwise, which a busy
    # node does not always manage. What counts is the restart, waited for
    # below.
    docker exec "$NODE" crictl --timeout 30s stop --timeout 0 "${cid#containerd://}" >/dev/null \
        || log "    crictl did not confirm the kill; waiting for the restart all the same"
    wait_for 120 "the worker to start again" dispatcher_running_after "$before" \
        || fail "the worker did not come back after being killed"
}

# =============================================================================
# Builds and pods
# =============================================================================

build_json() { aurcache_cli --format json builds get "$1" 2>/dev/null; }
build_status() { build_json "$1" | jq -r '.status'; }
build_output() { aurcache_cli builds output "$1" 2>/dev/null; }
build_said() { build_output "$1" | grep -qF -- "$2"; }

# The build pods, as `name uid build-id` lines.
build_pods() {
    k -n aurcache-builds get pods -l aurcache.io/build-id -o json \
        | jq -r '.items[] | "\(.metadata.name) \(.metadata.uid) \(.metadata.labels["aurcache.io/build-id"])"'
}
# Both false when the pods cannot be listed: a list nobody read holds no
# answer, and waiting on one of these then waits until it can be read.
no_build_pods() {
    local pods
    pods=$(build_pods) || return 1
    [ -z "$pods" ]
}
pod_uid_gone() {
    local pods
    pods=$(build_pods) || return 1
    ! grep -qF " $1 " <<<"$pods"
}

# Whether a build pod other than the one with uid `$1` exists.
another_pod() { build_pods | awk -v uid="$1" '$2 != uid' | grep -q .; }

build_ref_exists() { [ -n "$(latest_build_ref "$1")" ]; }

# Start a build of package `$1`: by adding it, with the rest of the
# arguments, in a cluster that has never built it, and by retrying its latest
# build in one that has (REUSE=1), where an add queues nothing and the latest
# build is an old one. What follows the package's source is passed to either,
# so `--wait` and its timeouts work for both.
build_package() {
    local pkg=$1 source=$2 before
    shift 2
    before=$(latest_build_ref "$pkg")
    if [ -z "$before" ]; then
        pkg_add "$source" "$@"
        return
    fi
    # What only an add takes says where the package comes from, which the
    # server already knows.
    local kept=()
    while (($#)); do
        case $1 in
            --ref | --platform) shift 2 ;;
            *) kept+=("$1"); shift ;;
        esac
    done
    log "    $pkg is here already; building it again from $before"
    aurcache_cli builds retry "$before" "${kept[@]}"
}

# `build_package`, in the background, as ADD_PID, with its output in `$1`;
# its exit status is the build's when it was asked to `--wait`.
build_in_background() {
    local out=$1
    shift
    build_package "$@" >"$out" 2>&1 &
    ADD_PID=$!
}

# Stop what `build_in_background` started. ADD_PID is the subshell `&` made;
# the CLI waiting on the build is a child of it, which killing the subshell
# alone leaves running.
stop_background() {
    [ -n "$ADD_PID" ] || return 0
    pkill -TERM -P "$ADD_PID" 2>/dev/null || true
    kill "$ADD_PID" 2>/dev/null || true
    ADD_PID=
}

# The newest build of package `$1` once it is not `$2`, the newest there was
# before a build was asked for (empty for a package with none).
new_build_ref() {
    local pkg=$1 before=$2 ref
    wait_for 60 "a new build of $pkg" ref_moved "$pkg" "$before" || return 1
    ref=$(latest_build_ref "$pkg")
    [ -n "$ref" ] && echo "$ref"
}

# Apply a pod from stdin, wait for it to end, print its log, delete it. True
# when it succeeded.
run_pod() {
    local ns=$1 name=$2 timeout=${3:-300} phase=
    k apply -f - >/dev/null
    local deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        phase=$(k -n "$ns" get pod "$name" -o jsonpath='{.status.phase}' 2>/dev/null || true)
        [ "$phase" = Succeeded ] || [ "$phase" = Failed ] && break
        sleep 2
    done
    k -n "$ns" logs "$name" --all-containers 2>&1 || true
    if [ "$phase" != Succeeded ] && [ "$phase" != Failed ]; then
        k -n "$ns" get events --field-selector "involvedObject.name=$name" \
            -o custom-columns=REASON:.reason,MESSAGE:.message --no-headers 2>/dev/null | tail -n 3
    fi
    k -n "$ns" delete pod "$name" --wait=false >/dev/null 2>&1 || true
    [ "$phase" = Succeeded ]
}

# =============================================================================
# The cluster
# =============================================================================

preflight() {
    local lacking=0 tool
    for tool in "$KIND" "$KUBECTL" docker jq curl cargo python3 ssh-keygen pkill; do
        command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: '$tool' is required but not found"; lacking=1; }
    done
    docker info >/dev/null 2>&1 || { echo "ERROR: cannot talk to the Docker daemon"; lacking=1; }
    [ "$lacking" -eq 0 ] || exit 1
    if [ "${E2E_SKIP_BUILD:-0}" != 1 ]; then
        require_packaging_submodules aurcache-sandbox aurcache-worker aurcache-server \
            aurcache-worker-docker
    fi
    if [ "${REUSE:-0}" = 1 ]; then
        "$KIND" get clusters 2>/dev/null | grep -qx "$CLUSTER" \
            || { echo "ERROR: REUSE=1, but there is no kind cluster named $CLUSTER"; exit 1; }
        return
    fi
    if "$KIND" get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
        echo "ERROR: a kind cluster named $CLUSTER exists already (a run left up?)."
        echo "       Delete it with: $KIND delete cluster --name $CLUSTER"
        exit 1
    fi
    if [ -d "$SCRATCH" ] && [ -n "$(ls -A "$SCRATCH")" ]; then
        echo "ERROR: $SCRATCH holds what an earlier run left. Remove it with:"
        echo "       docker run --rm --name $CLUSTER-cleanup -v $SCRATCH:/d --entrypoint rm $NODE_IMAGE -rf /d/containerd /d/kubelet /d/local-path"
        echo "       rm -rf $SCRATCH"
        exit 1
    fi
}

build_images() {
    if [ "${E2E_SKIP_BUILD:-0}" = 1 ]; then
        log "=== Using the images already built (E2E_SKIP_BUILD=1) ==="
        docker image inspect "$SERVER_IMAGE" "$WORKER_IMAGE" "$GITSSH_IMAGE" >/dev/null \
            || fail "E2E_SKIP_BUILD=1, but $SERVER_IMAGE, $WORKER_IMAGE or $GITSSH_IMAGE is missing"
        return
    fi
    log "=== Building the server and Kubernetes worker images ==="
    "$PROJECT_DIR/scripts/build-images.sh" --images server,kube-worker --platforms linux/amd64 \
        --tag "$TAG" "$REGISTRY"
    docker build -q -t "$GITSSH_IMAGE" -f "$PROJECT_DIR/docker/git-ssh.Dockerfile" \
        "$PROJECT_DIR" >/dev/null
}

create_cluster() {
    log "=== Creating kind cluster $CLUSTER (node data in $SCRATCH) ==="
    mkdir -p "$SCRATCH"/{containerd,kubelet,local-path}
    # The node keeps everything heavy under /var, on an anonymous volume of
    # the Docker daemon's unless mounted elsewhere: images, the pods' volumes
    # (a build's work directory) and the server's persistent volume.
    cat >"$SCRATCH/cluster.yaml" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    image: $NODE_IMAGE
    extraMounts:
      - hostPath: $SCRATCH/containerd
        containerPath: /var/lib/containerd
      - hostPath: $SCRATCH/kubelet
        containerPath: /var/lib/kubelet
      - hostPath: $SCRATCH/local-path
        containerPath: /var/local-path-provisioner
EOF
    CLUSTER_CREATED=1
    "$KIND" create cluster --name "$CLUSTER" --config "$SCRATCH/cluster.yaml" \
        --kubeconfig "$KUBECONFIG" --wait 180s
}

load_images() {
    log "=== Loading images into the node ==="
    "$KIND" load docker-image --name "$CLUSTER" "$SERVER_IMAGE" "$WORKER_IMAGE" "$GITSSH_IMAGE"
    if docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1; then
        log "    loading $BUILDER_IMAGE from the local Docker"
        # One platform only: from Docker's containerd image store, a pulled
        # image saves with an index naming every platform's manifest but the
        # blobs of one, which the node's import refuses.
        docker save --platform linux/amd64 -o "$SCRATCH/builder.tar" "$BUILDER_IMAGE"
        "$KIND" load image-archive --name "$CLUSTER" "$SCRATCH/builder.tar"
        rm -f "$SCRATCH/builder.tar"
    else
        log "    $BUILDER_IMAGE is not in the local Docker; the node pulls it"
    fi
}

# The run's own kustomization over the fixture: what differs per run.
write_overlay() {
    local dir="$SCRATCH/overlay" base token
    mkdir -p "$dir/packages"
    base=$(realpath --relative-to="$dir" "$FIXTURE")
    token=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
    cp "$FIXTURE/packages/kube-e2e-dep/PKGBUILD" "$dir/packages/kube-e2e-dep.PKGBUILD"
    cp "$FIXTURE/packages/kube-e2e-app/PKGBUILD" "$dir/packages/kube-e2e-app.PKGBUILD"
    cp "$FIXTURE/packages/kube-e2e-slow/PKGBUILD" "$dir/packages/kube-e2e-slow.PKGBUILD"
    cp "$PROJECT_DIR/tests/fixtures/packages/big/PKGBUILD" "$dir/packages/aurcache-big-test.PKGBUILD"
    cp "$FIXTURE/packages/kube-e2e-thief/PKGBUILD" "$dir/packages/kube-e2e-thief.PKGBUILD"
    cp "$FIXTURE/packages/kube-e2e-thief/kube-e2e-thief.install" "$dir/packages/kube-e2e-thief.install"
    cp "$FIXTURE/packages/kube-e2e-ssh/PKGBUILD" "$dir/packages/kube-e2e-ssh.PKGBUILD"
    # The dispatcher's SSH key, and the git server's host key, so that its
    # known_hosts can name it before the server runs.
    mkdir -p "$dir/ssh"
    rm -f "$dir/ssh/"*
    ssh-keygen -q -t ed25519 -N '' -C kube-e2e-dispatcher -f "$dir/ssh/id_ed25519"
    ssh-keygen -q -t ed25519 -N '' -C kube-e2e-gitssh -f "$dir/ssh/ssh_host_ed25519_key"
    echo "kube-e2e-gitssh.kube-e2e.svc $(cut -d' ' -f1-2 "$dir/ssh/ssh_host_ed25519_key.pub")" \
        >"$dir/ssh/known_hosts"
    cat >"$dir/kustomization.yaml" <<EOF
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - $base
images:
  - name: ghcr.io/lukas-heiligenbrunner/aurcache-server
    newName: $REGISTRY/aurcache-server
    newTag: $TAG
  - name: ghcr.io/lukas-heiligenbrunner/aurcache-worker-kube
    newName: $REGISTRY/aurcache-worker-kube
    newTag: $TAG
  - name: aurcache-gitssh
    newName: $REGISTRY/aurcache-gitssh
    newTag: $TAG
secretGenerator:
  - name: kube-e2e-gitssh
    namespace: kube-e2e
    files:
      - ssh/id_ed25519.pub
      - ssh/ssh_host_ed25519_key
      - ssh/ssh_host_ed25519_key.pub
    options:
      disableNameSuffixHash: true
  - name: aurcache-build-ssh
    namespace: aurcache
    files:
      - ssh-privatekey=ssh/id_ed25519
      - ssh/known_hosts
    options:
      disableNameSuffixHash: true
  - name: aurcache-enrollment
    namespace: aurcache
    literals:
      - token=$token
    options:
      disableNameSuffixHash: true
# The base copied its own image into WORKER_KUBE_AGENT_IMAGE before the
# images above were set, so it is copied again here.
replacements:
  - source:
      kind: Deployment
      name: aurcache-kube-worker
      fieldPath: spec.template.spec.containers.[name=worker].image
    targets:
      - select:
          kind: Deployment
          name: aurcache-kube-worker
        fieldPaths:
          - spec.template.spec.containers.[name=worker].env.[name=WORKER_KUBE_AGENT_IMAGE].value
configMapGenerator:
  - name: kube-e2e-packages
    namespace: kube-e2e
    files:
      - packages/kube-e2e-dep.PKGBUILD
      - packages/kube-e2e-app.PKGBUILD
      - packages/kube-e2e-slow.PKGBUILD
      - packages/aurcache-big-test.PKGBUILD
      - packages/kube-e2e-thief.PKGBUILD
      - packages/kube-e2e-thief.install
      - packages/kube-e2e-ssh.PKGBUILD
    options:
      disableNameSuffixHash: true
patches:
  - target:
      kind: Deployment
      name: aurcache-kube-worker
    patch: |-
      - op: add
        path: /spec/template/spec/containers/0/env/-
        value:
          name: WORKER_KUBE_BUILDER_IMAGE
          value: $BUILDER_IMAGE
      - op: add
        path: /spec/template/spec/containers/0/env/-
        value:
          name: WORKER_GIT_SSH_KEY
          value: /etc/aurcache-ssh/ssh-privatekey
      - op: add
        path: /spec/template/spec/containers/0/env/-
        value:
          name: WORKER_SSH_KNOWN_HOSTS
          value: /etc/aurcache-ssh/known_hosts
      - op: add
        path: /spec/template/spec/containers/0/volumeMounts/-
        value:
          name: build-ssh
          mountPath: /etc/aurcache-ssh
          readOnly: true
      - op: add
        path: /spec/template/spec/volumes/-
        value:
          name: build-ssh
          secret:
            secretName: aurcache-build-ssh
            defaultMode: 0440
  - target:
      kind: Deployment
      name: kube-e2e-git
    patch: |-
      - op: replace
        path: /spec/template/spec/initContainers/0/image
        value: $BUILDER_IMAGE
      - op: replace
        path: /spec/template/spec/containers/0/image
        value: $BUILDER_IMAGE
EOF
}

deploy() {
    log "=== Deploying the server, the git daemon and the Kubernetes worker ==="
    write_overlay
    k apply -k "$SCRATCH/overlay" >/dev/null
    k -n aurcache rollout status deploy/aurcache --timeout="$ROLLOUT" \
        || fail "the server did not become ready"
    k -n kube-e2e rollout status deploy/kube-e2e-gitssh --timeout="$ROLLOUT" \
        || fail "the SSH git server did not become ready"
    k -n kube-e2e rollout status deploy/kube-e2e-git --timeout="$ROLLOUT" \
        || fail "the git daemon did not become ready"
    k -n aurcache rollout status deploy/aurcache-kube-worker --timeout="$ROLLOUT" \
        || fail "the Kubernetes worker did not start"
}

# The API through a port-forward to the server's Service, on a free port.
# Started again whenever it has died.
ensure_forward() {
    if [ -n "$FORWARD_PID" ] && kill -0 "$FORWARD_PID" 2>/dev/null; then
        return
    fi
    local out="$SCRATCH/port-forward.log" port='' i
    # kubectl itself, not `k`: a function in the background is a subshell,
    # and killing it would leave the port-forward running.
    "$KUBECTL" -n aurcache port-forward svc/aurcache :8080 >"$out" 2>&1 &
    FORWARD_PID=$!
    for i in $(seq 50); do
        port=$(sed -n 's/^Forwarding from 127\.0\.0\.1:\([0-9]*\) .*/\1/p' "$out" | head -n 1)
        [ -n "$port" ] && break
        sleep 0.2
    done
    [ -n "$port" ] || { cat "$out"; echo "ERROR: the port-forward to the server did not start"; return 1; }
    export AURCACHE_PORT=$port
    export AURCACHE_URL="http://127.0.0.1:$port/api"
}

wait_for_service() {
    log "=== Waiting for the API ==="
    wait_for 120 "the API" aurcache_cli health >/dev/null || fail "the API did not answer"
}

worker_json() {
    aurcache_cli --format json worker list 2>/dev/null \
        | jq -c '[.[] | select(.kind == "kubernetes")] | first // empty'
}
worker_approved() { [ "$(worker_json | jq -r '.status // empty')" = approved ]; }

# =============================================================================
# The legs
# =============================================================================

check_enrollment() {
    log "=== 1. The worker enrolls and starts ==="
    wait_for 180 "the Kubernetes worker to be approved" worker_approved \
        || fail "no approved worker of kind kubernetes (the enrollment Secret should approve it)"
    local worker
    worker=$(worker_json)
    log "    worker: $(jq -r '"\(.name), \(.kind), \(.status), arches \(.arches | join(","))"' <<<"$worker")"
    result "enrolled through the Secret, approved, kind kubernetes"

    # The worker is told its own image for the build agent (what build pods
    # run is checked on one, in the SSH leg): it holds no permission to read
    # its pod.
    local line
    expect_said "Build pods go to namespace aurcache-builds and take the build agent from" "" 60
    line=$(dispatcher_log | grep -E "take the build agent from" | tail -n 1)
    log "    agent image: ${line##* from }"
    # `can-i` prints its answer; an error asking is neither.
    local may
    may=$(k auth can-i get pods -n aurcache \
        --as=system:serviceaccount:aurcache:aurcache-kube-worker 2>&1 || true)
    [ "$may" = no ] \
        || fail "the worker's ServiceAccount should not read pods in its own namespace; can-i said: $may"
    result "the worker names its agent image, and may read no pod beside the server"

    # The worker loaded its SSH key into an ssh-agent of its own as it
    # started, in the pod as the manifests make it: an unprivileged user, a
    # read-only root filesystem, no capability. And that pod is one a
    # namespace enforcing `restricted` admits: asked of the API server, with
    # the pod's own spec, in a namespace made for the question.
    expect_said "Builds fetch their sources with the SSH key" "" 10
    line=$(dispatcher_log | grep -E "Builds fetch their sources with the SSH key" | tail -n 1)
    grep -qE "\(256 SHA256:[A-Za-z0-9+/]+ .*\(ED25519\)\)" <<<"$line" \
        || fail "the worker did not say which key its ssh-agent has: $line"
    k create namespace kube-e2e-restricted --dry-run=client -o yaml | k apply -f - >/dev/null
    k label --overwrite namespace kube-e2e-restricted \
        pod-security.kubernetes.io/enforce=restricted \
        pod-security.kubernetes.io/enforce-version=latest >/dev/null
    local admitted
    admitted=$(k -n aurcache get pod "$(dispatcher_pod)" -o json \
        | jq '{apiVersion, kind, metadata: {name: "restricted-check"},
               spec: (.spec | del(.nodeName, .serviceAccountName, .serviceAccount))}' \
        | k -n kube-e2e-restricted create --dry-run=server -f - 2>&1) \
        || { k delete namespace kube-e2e-restricted --wait=false >/dev/null 2>&1; \
             fail "the worker's pod is not one \`restricted\` admits: $admitted"; }
    k delete namespace kube-e2e-restricted --wait=false >/dev/null 2>&1 || true
    result "the worker's ssh-agent has the key, and its pod passes \`restricted\`"
    expect_said "Build pods share their node's user namespace" "" 10
    result "warned that build pods share the node's user namespace"
    # In a cluster that ran before, the worker has been restarted under
    # builds on purpose.
    if [ "${REUSE:-0}" != 1 ] && ! dispatcher_silent "Deleted [0-9]+ build pod"; then
        fail "a first start found build pods to delete"
    fi
}

check_network_policy() {
    log "=== 2. The builds' NetworkPolicy ==="
    # The worker's own check, which runs beside enrollment and takes ~20s.
    if ! wait_for 180 "the worker's NetworkPolicy check" dispatcher_said \
        "The builds' NetworkPolicy (is enforced|is not enforced|was not checked)|Could not check whether the builds' NetworkPolicy"; then
        fail "the worker never reported its NetworkPolicy check"
    fi
    dispatcher_log | grep -E "NetworkPolicy" | sed 's/^/    | /'
    dispatcher_said "The builds' NetworkPolicy is enforced" \
        || fail "the worker did not find the NetworkPolicy enforced (kindnet enforces it)"
    result "the worker's probe: NetworkPolicy enforced"

    # The same question asked by hand, with a control outside the policy, so
    # that "enforced" cannot mean that nothing answers at all.
    local server_ip built control
    server_ip=$(k -n aurcache get pods -l app.kubernetes.io/component=server \
        -o jsonpath='{.items[0].status.podIP}')
    built=$(probe_pod aurcache-builds kube-e2e-probe-build "aurcache.aurcache.svc:8083" \
        "aurcache.aurcache.svc:8083" "$server_ip:8083" "aurcache.aurcache.svc:8081") \
        || fail "the labelled probe pod failed: $built"
    control=$(probe_pod kube-e2e kube-e2e-probe-control "" "aurcache.aurcache.svc:8083" \
        "$server_ip:8083" "aurcache.aurcache.svc:8081") || fail "the control probe pod failed: $control"
    log "    labelled like a build: $built"
    log "    outside the policy:    $control"
    reach() { jq -r --arg t "$2" '.[] | select(.target == $t) | .reachable' <<<"$1"; }
    [ "$(reach "$built" "aurcache.aurcache.svc:8083")" = false ] \
        || fail "a pod labelled like a build reached the server's worker port"
    [ "$(reach "$built" "$server_ip:8083")" = false ] \
        || fail "a pod labelled like a build reached the server pod's worker port"
    [ "$(reach "$built" "aurcache.aurcache.svc:8081")" = true ] \
        || fail "a pod labelled like a build could not reach the repository"
    [ "$(reach "$control" "aurcache.aurcache.svc:8083")" = true ] \
        || fail "the control pod could not reach the worker port, so the check proves nothing"
    [ "$(reach "$control" "$server_ip:8083")" = true ] \
        || fail "the control pod could not reach the server pod's worker port, so the check proves nothing"
    [ "$(reach "$control" "aurcache.aurcache.svc:8081")" = true ] \
        || fail "the control pod could not reach the repository"
    result "by hand: a build pod reaches the repository and not the worker port; the control reaches both"
}

# The agent's `probe` from a pod named `$2` in namespace `$1`, labelled like
# a build there, of the targets from `$4` on; prints its one line of JSON.
#
# `$3` is a target the policy is to keep the pod from, or nothing for a pod
# no policy selects. kindnet puts the policy around a pod only once it has
# seen the pod, a moment after its containers start, and lets everything
# through until then. So the pod asks again, for a minute at most, until that
# target does not answer, and prints what it found last: fenced when the
# policy works, and still open, for the caller to fail on, when it does not.
# The worker's own probe asks again for the same reason.
probe_pod() {
    local ns=$1 name=$2 fenced=$3 out
    shift 3
    local args=""
    for target in "$@"; do args+=" --connect $target"; done
    local probe="/usr/local/bin/aurcache-build-agent probe --timeout 5$args" script closed
    script="exec $probe"
    if [ -n "$fenced" ]; then
        # The line the agent prints for that target once it does not answer.
        closed="{\"target\":\"$fenced\",\"reachable\":false}"
        # For the pod's shell to expand, not this one.
        # shellcheck disable=SC2016
        script='i=0; while :; do out=$('"$probe"'); case "$out" in *'"'$closed'"'*) break ;; esac; i=$((i + 1)); [ $i -lt 12 ] || break; sleep 5; done; echo "$out"'
    fi
    out=$(run_pod "$ns" "$name" 180 <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  namespace: $ns
  labels:
    app.kubernetes.io/managed-by: aurcache-worker-kube
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
  securityContext:
    runAsNonRoot: true
    runAsUser: 65532
    seccompProfile:
      type: RuntimeDefault
  containers:
    - name: probe
      image: $WORKER_IMAGE
      imagePullPolicy: IfNotPresent
      command:
        - sh
        - -c
        - |
          $script
      securityContext:
        allowPrivilegeEscalation: false
        capabilities:
          drop: [ALL]
EOF
    ) || { echo "$out"; return 1; }
    grep -E '^\[' <<<"$out" | tail -n 1
}

check_ssh_key() {
    log "=== 4b. A source over git+ssh with the dispatcher's key, and a dependency that hunts for it ==="
    CURRENT_BUILD_REF=
    build_package kube-e2e-thief "$GIT_URL/kube-e2e-thief.git" --ref master \
        --wait --wait-timeout "$BUILD_TIMEOUT" \
        | sed 's/^/    /' || fail "kube-e2e-thief did not build"
    # The build's pod goes with the build, so what it is made of is read while
    # it is there: every build pod seen, the first time there is one. The
    # thief's pod may still be going, and is waited for so that it is not
    # the one seen.
    wait_for 60 "the pod of kube-e2e-thief to go" no_build_pods \
        || fail "the pod of kube-e2e-thief outlived its build by a minute"
    local pid pods=''
    build_in_background "$SCRATCH/add-kube-e2e-ssh.log" kube-e2e-ssh \
        "$GIT_URL/kube-e2e-ssh.git" --ref master --wait --wait-timeout "$BUILD_TIMEOUT"
    pid=$ADD_PID
    while kill -0 "$pid" 2>/dev/null; do
        [ -n "$pods" ] || pods=$(k -n aurcache-builds get pods -l aurcache.io/build-id -o json \
            2>/dev/null | jq -c '.items | select(length > 0)' 2>/dev/null || true)
        sleep 2
    done
    if ! wait "$pid"; then
        ADD_PID=
        sed 's/^/    /' "$SCRATCH/add-kube-e2e-ssh.log"
        CURRENT_BUILD_REF=$(latest_build_ref kube-e2e-ssh)
        fail "kube-e2e-ssh did not build"
    fi
    ADD_PID=
    sed 's/^/    /' "$SCRATCH/add-kube-e2e-ssh.log"
    local ref output found
    ref=$(latest_build_ref kube-e2e-ssh)
    CURRENT_BUILD_REF=$ref
    output=$(build_output "$ref")
    grep -E "SSH link|kube-e2e-ssh:|downloading the sources" <<<"$output" | sed 's/^/    | /'
    grep -qF "kube-e2e-ssh: payload aurcache-ssh-credential-reached-the-chroot" <<<"$output" \
        || fail "$ref did not fetch its git+ssh source"
    grep -qE "\[worker\] the SSH link closed: [1-9][0-9]* signature" <<<"$output" \
        || fail "the log of $ref says no SSH link closed having signed"
    # The build has the agent for as long as it runs, and its dependency's
    # root script reaches it too. Neither finds the key, where each finds the
    # canary it planted in a file and in an environment, so that finding no
    # key says something. Both can have it sign what is no login: ssh-agent
    # signs what it is asked to, which is why the key must not be trusted as
    # a signing key anywhere.
    local built canary='canary-files=[1-9][0-9]*\ canary-environs=[1-9][0-9]*'
    built=$(grep -oE "kube-e2e-ssh: build .*" <<<"$output" | tail -n 1)
    [[ $built =~ ^kube-e2e-ssh:\ build\ ssh=reached\ keys=0\ $canary\ sshsig=signed[[:space:]]*$ ]] \
        || fail "the build's report in $ref is not 'reached the server, no key, its canary found, an SSHSIG made': $built"
    found=$(grep -oE "kube-e2e-ssh: thief .*" <<<"$output" | tail -n 1)
    [[ $found =~ ^kube-e2e-ssh:\ thief\ ran=yes\ keys=0\ $canary\ agents=[1-9][0-9]*\ sshsig=signed[[:space:]]*$ ]] \
        || fail "the thief's report in $ref is not 'ran, no key, its canary found, reached the agent, an SSHSIG made': $found"
    # No Secret reaches the pod: not as a volume, not as a projected source
    # (a service account token is one), not in an environment.
    [ -n "$pods" ] || fail "no build pod of $ref was seen while it ran"
    local secrets agents
    secrets=$(jq '[.[].spec
        | ((.volumes // [])[]
            | select(.secret != null
                or any((.projected.sources // [])[]; .secret != null or .serviceAccountToken != null))),
          (((.containers // []) + (.initContainers // []))[]
            | select(any((.envFrom // [])[]; .secretRef != null)
                or any((.env // [])[]; .valueFrom.secretKeyRef != null)))]
        | length' <<<"$pods")
    [ "$secrets" = 0 ] || fail "a build pod of $ref is given a Secret: $(jq -c '[.[].spec.volumes]' <<<"$pods")"
    # The agent the pod ran, in `stage` and `hold`, is the worker image this
    # run built.
    agents=$(jq -r '[.[].spec | ((.initContainers // []) + .containers)[]
        | select(.name == "stage" or .name == "hold") | .image] | unique | join(" ")' <<<"$pods")
    [ "$agents" = "$WORKER_IMAGE" ] \
        || fail "the build pod of $ref took its agent from $agents, not from $WORKER_IMAGE"
    result "the pod of $ref was given no Secret, and ran the agent of $WORKER_IMAGE"
    result "$ref fetched over git+ssh through the dispatcher, from build() too; no key in the pod, and the agent signs what it is asked to: $found"
}

check_quota_then_build() {
    log "=== 3. A full quota holds claims back; then $PACKAGE builds ==="
    local since pid ref
    local before
    before=$(latest_build_ref "$PACKAGE")
    since=$(now)
    k -n aurcache-builds delete quota kube-e2e-full --ignore-not-found >/dev/null
    k -n aurcache-builds create quota kube-e2e-full --hard=pods=0 >/dev/null
    build_in_background "$SCRATCH/add-$PACKAGE.log" "$PACKAGE" "$PACKAGE" --platform x86_64 \
        --wait --wait-timeout "$BUILD_TIMEOUT" --wait-stall-after 600
    pid=$ADD_PID
    expect_said "Not claiming builds: the cluster refuses a build pod" "$since" 120
    ref=$(new_build_ref "$PACKAGE" "$before") || fail "no build of $PACKAGE was queued"
    CURRENT_BUILD_REF=$ref
    # A claim would show as a pod, refused here, and as a build no longer
    # queued.
    sleep 15
    [ "$(build_status "$ref")" = 3 ] || fail "$ref left the queue while the quota was full"
    no_build_pods || fail "a build pod exists while the quota admits none"
    result "quota full: the worker said so and $ref stayed queued"

    since=$(now)
    k -n aurcache-builds delete quota kube-e2e-full >/dev/null
    expect_said "Claiming builds again" "$since" 120
    if ! wait "$pid"; then
        ADD_PID=
        sed 's/^/    /' "$SCRATCH/add-$PACKAGE.log"
        fail "$ref did not build"
    fi
    ADD_PID=
    result "quota gone: claiming again, and $ref built"

    local output json
    output=$(build_output "$ref")
    for line in "[worker] created pod" "[worker] handing the job to the pod" "==> Finished making"; do
        grep -qF -- "$line" <<<"$output" || fail "the log of $ref has no '$line'"
    done
    json=$(build_json "$ref")
    log "    $(jq -c '{size, peak_memory, disk_usage, worker_name}' <<<"$json")"
    [ "$(jq -r '.peak_memory // empty' <<<"$json")" != "" ] || fail "$ref reports no peak memory"
    [ "$(jq -r '.disk_usage // empty' <<<"$json")" != "" ] || fail "$ref reports no disk use"
    result "$ref: build output in its log, peak memory $(jq -r .peak_memory <<<"$json") B, disk use reported"
    wait_for 60 "the build pod of $ref to go" no_build_pods || fail "the pod of $ref was left behind"
}

check_repository() {
    log "=== 4. A dependency from AURCache's own repository ==="
    CURRENT_BUILD_REF=
    build_package kube-e2e-dep "$GIT_URL/kube-e2e-dep.git" --ref master \
        --wait --wait-timeout "$BUILD_TIMEOUT" \
        | sed 's/^/    /' || fail "kube-e2e-dep did not build"
    CURRENT_BUILD_REF=$(latest_build_ref kube-e2e-dep)
    build_package kube-e2e-app "$GIT_URL/kube-e2e-app.git" --ref master \
        --wait --wait-timeout "$BUILD_TIMEOUT" \
        | sed 's/^/    /' || { CURRENT_BUILD_REF=$(latest_build_ref kube-e2e-app); fail "kube-e2e-app did not build"; }
    local ref
    ref=$(latest_build_ref kube-e2e-app)
    CURRENT_BUILD_REF=$ref
    build_said "$ref" "kube-e2e-app: found kube-e2e-dep's marker" \
        || fail "the build of $ref did not find its dependency installed"
    build_output "$ref" | grep -E "kube-e2e-dep" | head -n 3 | sed 's/^/    | /'
    result "$ref built against kube-e2e-dep installed from AURCache's repository"

    log "    installing $PACKAGE and kube-e2e-app from the repository in a pod"
    local out
    out=$(run_pod kube-e2e kube-e2e-install 600 <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: kube-e2e-install
  namespace: kube-e2e
spec:
  restartPolicy: Never
  securityContext:
    runAsUser: 0
  containers:
    - name: install
      image: $BUILDER_IMAGE
      imagePullPolicy: IfNotPresent
      command:
        - sh
        - -euc
        - |
          printf '[repo]\nSigLevel = Optional TrustAll\nServer = http://aurcache.aurcache.svc:8081/\$arch\n' >> /etc/pacman.conf
          pacman -Sy --noconfirm "\$@" >/dev/null
          pacman -Q "\$@"
          cat /usr/share/kube-e2e-dep/marker /usr/share/kube-e2e-app/marker
        - install
        - $PACKAGE
        - kube-e2e-app
EOF
    ) || { echo "$out" | tail -n 20; fail "installing from the repository failed"; }
    printf '%s\n' "$out" | sed 's/^/    | /'
    grep -q "kube-e2e-app marker" <<<"$out" || fail "kube-e2e-app installed without its files"
    result "$PACKAGE, kube-e2e-app and kube-e2e-dep installed with pacman from the repository"
}

# The build of `$1` running its build(): its log says so.
running_build() { build_said "$1" "kube-e2e-slow: sleeping"; }

check_cancel() {
    log "=== 5. Cancelling a build ==="
    CURRENT_BUILD_REF=
    local before ref started
    before=$(latest_build_ref kube-e2e-slow)
    build_package kube-e2e-slow "$GIT_URL/kube-e2e-slow.git" --ref master | sed 's/^/    /' \
        || fail "starting a build of kube-e2e-slow failed"
    ref=$(new_build_ref kube-e2e-slow "$before") || fail "no build of kube-e2e-slow was queued"
    CURRENT_BUILD_REF=$ref
    wait_for "$BUILD_TIMEOUT" "$ref to be building" running_build "$ref" \
        || fail "$ref never started building"
    started=$SECONDS
    aurcache_cli builds cancel "$ref" | sed 's/^/    /'
    wait_for 60 "the pod of $ref to go" no_build_pods || fail "the pod of $ref outlived its cancel by a minute"
    log "    the pod was gone $((SECONDS - started))s after the cancel"
    wait_for 60 "$ref to end" build_over "$ref" || fail "$ref did not end after its cancel"
    result "$ref cancelled; its pod gone after $((SECONDS - started))s"
}

build_over() {
    local status
    status=$(build_status "$1")
    [ "$status" = 1 ] || [ "$status" = 2 ]
}

check_restart() {
    log "=== 6. The worker dies under a build ==="
    local before ref pod uid since
    before=$(latest_build_ref kube-e2e-slow)
    aurcache_cli builds retry "$before" | sed 's/^/    /'
    wait_for 60 "a new build of kube-e2e-slow" ref_moved kube-e2e-slow "$before" \
        || fail "the retry queued nothing"
    ref=$(latest_build_ref kube-e2e-slow)
    CURRENT_BUILD_REF=$ref
    wait_for "$BUILD_TIMEOUT" "$ref to be building" running_build "$ref" \
        || fail "$ref never started building"
    read -r pod uid _ <<<"$(build_pods | head -n 1)"
    log "    $ref runs in $pod ($uid)"

    since=$(now)
    crash_dispatcher
    expect_said "Deleted 1 build pod\(s\) left by a previous run of this worker" "$since" 120
    wait_for 60 "pod $uid to go" pod_uid_gone "$uid" || fail "the old build pod survived the sweep"
    result "worker killed mid-build: the new process deleted $pod"

    # The server ends the build once the new process says it holds none, and
    # queues a new build of the package while it has attempts left.
    wait_for 180 "kube-e2e-slow to be built again in a new pod" another_pod "$uid" \
        || fail "the server did not queue kube-e2e-slow again, or the worker did not claim it"
    local again
    again=$(latest_build_ref kube-e2e-slow)
    log "    now building: $again in $(build_pods | awk '{print $1}' | head -n 1)"
    [ "$again" != "$ref" ] || fail "no build of kube-e2e-slow newer than $ref"
    log "    $ref: $(build_output "$ref" | grep -v '^\s*$' | tail -n 1)"
    build_over "$ref" || fail "$ref is still going beside $again"
    result "the server ended $ref and queued $again, which the worker started in a new pod"
    CURRENT_BUILD_REF=$again
    aurcache_cli builds cancel "$again" >/dev/null
    wait_for 60 "the pod of $again to go" no_build_pods || fail "the pod of $again outlived its cancel"
}

# Whether no pod named `$1` is in the server's namespace any more.
dispatcher_pod_gone() { ! k -n aurcache get pod "$1" >/dev/null 2>&1; }

# Whether process `$1` has ended.
process_gone() { ! kill -0 "$1" 2>/dev/null; }

check_stop() {
    log "=== 7. The worker is stopped under a build ==="
    local before ref pod uid old said took
    before=$(latest_build_ref kube-e2e-slow)
    aurcache_cli builds retry "$before" | sed 's/^/    /'
    wait_for 60 "a new build of kube-e2e-slow" ref_moved kube-e2e-slow "$before" \
        || fail "the retry queued nothing"
    ref=$(latest_build_ref kube-e2e-slow)
    CURRENT_BUILD_REF=$ref
    wait_for "$BUILD_TIMEOUT" "$ref to be building" running_build "$ref" \
        || fail "$ref never started building"
    read -r pod uid _ <<<"$(build_pods | head -n 1)"
    log "    $ref runs in $pod ($uid)"

    # The pod's log goes with the pod, so it is followed from before: by
    # kubectl itself, which the cleanup can stop, and once it has written
    # the log so far, which the worker's start never leaves empty, so that
    # it is following before the pod is told to stop.
    old=$(dispatcher_pod)
    said="$SCRATCH/stopped-worker.log"
    : >"$said"
    "$KUBECTL" -n aurcache logs -f "$old" -c worker >"$said" 2>"$said.err" &
    FOLLOW_PID=$!
    wait_for 30 "the log of $old to be followed" test -s "$said" \
        || fail "the log of $old could not be followed: $(cat "$said.err")"
    local started=$SECONDS
    k -n aurcache delete pod "$old" --wait=false >/dev/null
    # Its grace period is 60s: a worker that did not act on the signal is
    # killed at the end of it, and not before. One that did is gone in a few
    # seconds; the rest of the wait is for a machine doing other things.
    wait_for 54 "the worker's pod $old to stop" dispatcher_pod_gone "$old" \
        || fail "the worker's pod was still there 54s after it was deleted: it did not stop when told to"
    took=$((SECONDS - started))
    # The container is gone, and its log has ended with it.
    wait_for 30 "the log of $old to end" process_gone "$FOLLOW_PID" \
        || fail "following the log of $old did not end with its pod"
    wait "$FOLLOW_PID" 2>/dev/null || true
    FOLLOW_PID=
    sed 's/\x1b\[[0-9;]*m//g' "$said" | grep -E "Received SIGTERM" | sed 's/^/    | /' \
        || fail "the stopped worker never logged that it received SIGTERM"
    # The stopped worker deleted the pod itself, rather than leave it to its
    # successor's sweep, which deletes by the same label.
    sed 's/\x1b\[[0-9;]*m//g' "$said" | grep -E "deleted pod $pod\b" | sed 's/^/    | /' \
        || fail "the stopped worker did not log deleting $pod"
    wait_for 30 "pod $uid to go" pod_uid_gone "$uid" || fail "the build pod outlived the worker that ran it"
    result "worker pod deleted mid-build: it stopped in ${took}s, and deleted $pod itself"

    # The build is over on the server, by the stopped worker's own report or
    # by its successor's first heartbeat, whichever came first; a new build
    # of the package is queued, and the new worker pod starts it.
    k -n aurcache rollout status deploy/aurcache-kube-worker --timeout="$ROLLOUT" \
        || fail "the worker did not start again"
    wait_for 120 "$ref to end" build_over "$ref" || fail "$ref was not ended when its worker stopped"
    log "    $ref: $(build_output "$ref" | grep -v '^\s*$' | tail -n 1)"
    wait_for 180 "a new build of kube-e2e-slow in a new pod" another_pod "$uid" \
        || fail "the server did not queue kube-e2e-slow again, or the worker did not claim it"
    # Claiming comes after the startup sweep, so by now the new worker would
    # have said if it had found its predecessor's pod.
    dispatcher_silent "Deleted [0-9]+ build pod\(s\) left by a previous run" \
        || fail "the new worker found the build pod its stopped predecessor should have deleted"
    local again
    again=$(latest_build_ref kube-e2e-slow)
    [ "$again" != "$ref" ] || fail "no build of kube-e2e-slow newer than $ref"
    result "the server ended $ref and queued $again, which the new worker pod started"
    CURRENT_BUILD_REF=$again
    aurcache_cli builds cancel "$again" >/dev/null
    wait_for 60 "the pod of $again to go" no_build_pods || fail "the pod of $again outlived its cancel"
}

# The last line of the output of build `$1` that is not blank.
last_word() { build_output "$1" | grep -v '^\s*$' | tail -n 1; }

build_succeeded() { [ "$(build_status "$1")" = 1 ]; }

# Whether the newest build of package `$1` succeeded.
newest_succeeded() { build_succeeded "$(latest_build_ref "$1")"; }

check_stuck() {
    log "=== 8. A build pod that cannot start ==="
    local since before ref
    since=$(now)
    # A RuntimeClass whose handler the node does not have, through the pod
    # template: the pod is scheduled, and its sandbox is never made.
    k apply -f - >/dev/null <<EOF
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: kube-e2e-no-such-handler
handler: kube-e2e-no-such-handler
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: kube-e2e-stuck-template
  namespace: aurcache
data:
  pod-template.json: '{"runtimeClassName": "kube-e2e-no-such-handler"}'
EOF
    # A strategic merge: the volume and its mount are named, so a cluster
    # this ran in before takes the same patch again and changes nothing.
    k -n aurcache patch deploy/aurcache-kube-worker --type strategic -p '{"spec": {"template": {"spec": {
      "volumes": [{"name": "stuck-template", "configMap": {"name": "kube-e2e-stuck-template"}}],
      "containers": [{"name": "worker", "volumeMounts": [
        {"name": "stuck-template", "mountPath": "/etc/kube-e2e-stuck", "readOnly": true}]}]
    }}}}' >/dev/null
    k -n aurcache set env deploy/aurcache-kube-worker \
        WORKER_KUBE_POD_TEMPLATE=/etc/kube-e2e-stuck/pod-template.json \
        WORKER_KUBE_SCHEDULE_TIMEOUT=30s >/dev/null
    k -n aurcache rollout status deploy/aurcache-kube-worker --timeout="$ROLLOUT" \
        || fail "the worker did not start again"
    expect_said "take the build agent from" "$since" 120
    wait_for 120 "the worker to be approved again" worker_approved || fail "the worker did not come back"

    before=$(latest_build_ref kube-e2e-dep)
    aurcache_cli builds retry "$before" | sed 's/^/    /'
    wait_for 60 "a new build of kube-e2e-dep" ref_moved kube-e2e-dep "$before" \
        || fail "the retry queued nothing"
    ref=$(latest_build_ref kube-e2e-dep)
    CURRENT_BUILD_REF=$ref
    # Well inside the build's own timeout, which is what it used to wait for.
    wait_for 150 "$ref to be handed back" build_over "$ref" \
        || fail "$ref was still waiting for its pod 150s on, with a 30s allowance to start"
    log "    $ref: $(last_word "$ref")"
    build_said "$ref" "did not start within 30s" \
        || fail "the log of $ref does not say its pod did not start"
    k -n aurcache-builds get events --field-selector reason=FailedCreatePodSandBox \
        -o custom-columns=MESSAGE:.message --no-headers 2>/dev/null | tail -n 1 | sed 's/^/    | /'
    result "$ref: its pod never started, and the build was handed back as the worker's"

    since=$(now)
    k -n aurcache set env deploy/aurcache-kube-worker \
        WORKER_KUBE_POD_TEMPLATE- WORKER_KUBE_SCHEDULE_TIMEOUT- >/dev/null
    k -n aurcache rollout status deploy/aurcache-kube-worker --timeout="$ROLLOUT" \
        || fail "the worker did not start again"
    expect_said "take the build agent from" "$since" 120
    wait_for 120 "the worker to be approved again" worker_approved || fail "the worker did not come back"
    # The server queued the package again each time; with the worker put
    # right, the build that is queued now goes through.
    wait_for "$BUILD_TIMEOUT" "kube-e2e-dep to build again" newest_succeeded kube-e2e-dep \
        || fail "kube-e2e-dep did not build once the worker was put right"
    CURRENT_BUILD_REF=$(latest_build_ref kube-e2e-dep)
    wait_for 60 "the build pods to go" no_build_pods || fail "a build pod was left"
    result "$CURRENT_BUILD_REF built once the RuntimeClass was no longer asked for"
}

# Whether package `$1` has a build newer than `$2`.
ref_moved() { [ "$(latest_build_ref "$1")" != "$2" ]; }

check_big() {
    log "=== 9. A package of 2 GiB ==="
    if [ "${E2E_BIG:-1}" = 0 ]; then
        result "skipped: the 2 GiB package (E2E_BIG=0)"
        return
    fi
    local pid finished='' restarts json ref size end seconds peak
    restarts=$(dispatcher_restarts)
    CURRENT_BUILD_REF=
    build_in_background "$SCRATCH/add-big.log" aurcache-big-test \
        "$GIT_URL/aurcache-big-test.git" --ref master \
        --wait --wait-timeout 3600 --wait-stall-after 600
    pid=$ADD_PID
    # When the build container ended: from there on it is the relay.
    while kill -0 "$pid" 2>/dev/null; do
        if [ -z "$finished" ]; then
            finished=$(k -n aurcache-builds get pods -l aurcache.io/build-id -o json 2>/dev/null \
                | jq -r '[.items[].status.containerStatuses[]? | select(.name == "build")
                          | .state.terminated.finishedAt // empty] | first // empty')
        fi
        sleep 2
    done
    ref=$(latest_build_ref aurcache-big-test)
    CURRENT_BUILD_REF=$ref
    if ! wait "$pid"; then
        ADD_PID=
        tail -n 20 "$SCRATCH/add-big.log" | sed 's/^/    /'
        fail "$ref did not build"
    fi
    ADD_PID=
    json=$(build_json "$ref")
    size=$(jq -r '.size' <<<"$json")
    end=$(jq -r '.end_time' <<<"$json")
    if [ -n "$finished" ]; then
        seconds=$((end - $(date -d "$finished" +%s)))
        ((seconds > 0)) || seconds=1
        log "    $ref: $((size / 1024 / 1024)) MiB relayed and published in ${seconds}s after the build ended"
        result "$ref: $((size / 1024 / 1024)) MiB, ~$((size / seconds / 1024 / 1024)) MiB/s from the build's end to published"
    else
        result "$ref: $((size / 1024 / 1024)) MiB published (the build's end was not seen, so no throughput)"
    fi
    ((size > 2 * 1024 * 1024 * 1024)) || fail "$ref published only $size bytes"

    # The container's own cgroup, which its memory limit says it is.
    local limit
    limit=$(k -n aurcache exec deploy/aurcache-kube-worker -c worker -- cat /sys/fs/cgroup/memory.max)
    peak=$(k -n aurcache exec deploy/aurcache-kube-worker -c worker -- cat /sys/fs/cgroup/memory.peak)
    log "    the worker container's peak memory: $((peak / 1024 / 1024)) MiB, of its limit of $limit"
    [ "$limit" = "$((256 * 1024 * 1024))" ] \
        || fail "the worker's cgroup has limit $limit, not the manifest's 256Mi: the wrong cgroup?"
    [ "$(dispatcher_restarts)" = "$restarts" ] || fail "the worker restarted during the relay"
    # Well under the container's own limit, at which the kernel would have
    # killed it and this would never be reached.
    ((peak < MAX_WORKER_MEMORY)) || fail "the worker's peak memory, $peak bytes, reached $MAX_WORKER_MEMORY"
    result "the worker container's peak memory: $((peak / 1024 / 1024)) MiB (limit for the run $((MAX_WORKER_MEMORY / 1024 / 1024)) MiB)"
}

check_user_namespaces() {
    log "=== 10. Build pods in user namespaces ==="
    if [ "${E2E_USER_NAMESPACES:-auto}" = 0 ]; then
        result "skipped: user namespaces (E2E_USER_NAMESPACES=0)"
        return
    fi
    local out
    if ! out=$(run_pod kube-e2e kube-e2e-userns 90 <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: kube-e2e-userns
  namespace: kube-e2e
spec:
  restartPolicy: Never
  hostUsers: false
  containers:
    - name: userns
      image: $WORKER_IMAGE
      imagePullPolicy: IfNotPresent
      command: ["cat", "/proc/self/uid_map"]
EOF
    ); then
        local reason
        # The event, without the runtime's wrapping of it.
        reason=$(grep -vE '^\s*$' <<<"$out" | tail -n 1 | sed 's/.*error during container init: //')
        if [ "${E2E_USER_NAMESPACES:-auto}" = 1 ]; then
            fail "the node cannot run a pod with hostUsers: false: $reason"
        fi
        result "skipped: user namespaces; the node cannot run a pod with hostUsers: false: $reason"
        return
    fi
    log "    a pod with hostUsers: false maps: $(tr -s ' ' <<<"$out" | head -n 1)"

    local since ref uid hostusers=
    since=$(now)
    k -n aurcache set env deploy/aurcache-kube-worker WORKER_KUBE_USER_NAMESPACES=true >/dev/null
    k -n aurcache rollout status deploy/aurcache-kube-worker --timeout="$ROLLOUT" \
        || fail "the worker did not start again with user namespaces"
    expect_said "take the build agent from" "$since" 120
    dispatcher_silent "Build pods share their node's user namespace" "$since" \
        || fail "the worker still warns about user namespaces with them on"
    wait_for 120 "the worker to be approved again" worker_approved || fail "the worker did not come back"
    local before
    before=$(latest_build_ref kube-e2e-dep)
    aurcache_cli builds retry "$before" | sed 's/^/    /'
    wait_for 60 "a new build of kube-e2e-dep" ref_moved kube-e2e-dep "$before" \
        || fail "the retry queued nothing"
    ref=$(latest_build_ref kube-e2e-dep)
    CURRENT_BUILD_REF=$ref
    while ! build_over "$ref"; do
        [ -n "$hostusers" ] || hostusers=$(k -n aurcache-builds get pods -l aurcache.io/build-id \
            -o jsonpath='{.items[0].spec.hostUsers}' 2>/dev/null || true)
        sleep 2
    done
    [ "$(build_status "$ref")" = 1 ] || fail "$ref failed with user namespaces on"
    [ "$hostusers" = false ] || fail "the build pod of $ref did not ask for a user namespace (hostUsers: '$hostusers')"
    result "$ref built in a pod with hostUsers: false"
    # As it was, for the legs after this one and for a run after this one.
    k -n aurcache set env deploy/aurcache-kube-worker WORKER_KUBE_USER_NAMESPACES- >/dev/null
    k -n aurcache rollout status deploy/aurcache-kube-worker --timeout="$ROLLOUT" \
        || fail "the worker did not start again without user namespaces"
    wait_for 120 "the worker to be approved again" worker_approved || fail "the worker did not come back"
}

# The latest of the server's log at severity `$1`, as text. Fails when the
# log cannot be read, which is not the same as holding nothing.
server_log() {
    local body
    body=$(curl -fsS "$AURCACHE_URL/log?severity=$1&limit=100") || return 1
    jq -r '.. | strings' <<<"$body"
}

# Whether the server's log has `$2` at severity `$1`, among its latest. For
# waiting until it does: a log that cannot be read does not have it yet.
server_logged() {
    local text
    text=$(server_log "$1" 2>/dev/null) || return 1
    grep -qF -- "$2" <<<"$text"
}

check_open_policy() {
    log "=== 11. Without the NetworkPolicy ==="
    local since
    k -n aurcache-builds delete networkpolicy aurcache-builds --ignore-not-found >/dev/null
    # The SSH leg's policy selects build pods too, and would keep them fenced.
    # Kept from the first time, for a run that stopped before putting it back.
    if k -n aurcache-builds get networkpolicy kube-e2e-builds-to-gitssh >/dev/null 2>&1; then
        k -n aurcache-builds get networkpolicy kube-e2e-builds-to-gitssh -o yaml \
            >"$SCRATCH/gitssh-policy.yaml"
        k -n aurcache-builds delete networkpolicy kube-e2e-builds-to-gitssh >/dev/null
    fi
    since=$(now)
    crash_dispatcher
    expect_said "The builds' NetworkPolicy is not enforced" "$since" 180
    result "the worker says the NetworkPolicy is not enforced"
    ensure_forward
    wait_for 90 "the server to hear of it" server_logged error \
        "The builds' NetworkPolicy is not enforced" \
        || fail "the server's log has no error saying the NetworkPolicy is not enforced"
    result "the server's log has it at error severity"
    # Put back, and the worker started once more so that what it last said of
    # the policy is true of the cluster as it is left.
    k apply -f "$PROJECT_DIR/kubernetes/kube-worker/networkpolicy.yaml" >/dev/null
    k apply -f "$SCRATCH/gitssh-policy.yaml" >/dev/null
    since=$(now)
    crash_dispatcher
    expect_said "The builds' NetworkPolicy is enforced" "$since" 180
}

# =============================================================================
# Failure and cleanup
# =============================================================================

dump_logs_on_failure() {
    [ "$CLUSTER_CREATED" = 1 ] || return 0
    {
        echo "=== pods ==="
        k get pods -A -o wide
        echo "=== events in aurcache-builds ==="
        k -n aurcache-builds get events --sort-by=.lastTimestamp
        echo "=== the worker ==="
        k -n aurcache logs deploy/aurcache-kube-worker -c worker --timestamps
        echo "=== the worker, its previous container ==="
        k -n aurcache logs deploy/aurcache-kube-worker -c worker --previous --timestamps
        echo "=== the server ==="
        k -n aurcache logs deploy/aurcache -c aurcache --timestamps
        echo "=== the git daemon ==="
        k -n kube-e2e logs deploy/kube-e2e-git --all-containers --timestamps
    } >"$LOG_FILE" 2>&1 || true

    echo "--- builds ---"
    aurcache_cli builds list --limit 20 2>/dev/null || echo "    (unavailable)"
    if [ -n "$CURRENT_BUILD_REF" ]; then
        echo "--- build $CURRENT_BUILD_REF: last 25 lines ---"
        build_output "$CURRENT_BUILD_REF" | tail -n 25 || echo "    (unavailable)"
    fi
    echo "--- the worker: errors and warnings ---"
    dispatcher_log | grep -E " (ERROR|WARN) " | tail -n 15 || echo "    (none)"
    echo "--- the server: errors ---"
    k -n aurcache logs deploy/aurcache -c aurcache 2>/dev/null | filter_noise | grep -E " ERROR " | tail -n 10 || echo "    (none)"
    echo
    echo "    Full logs: $LOG_FILE"
}

cleanup() {
    local exit_code=$? do_cleanup
    if [ "${CLEANUP:-}" = 1 ]; then
        do_cleanup=1
    elif [ "${CLEANUP:-}" = 0 ]; then
        do_cleanup=0
    elif [ "$exit_code" -eq 0 ]; then
        do_cleanup=1
    else
        do_cleanup=0
    fi
    if [ -n "$FORWARD_PID" ]; then kill "$FORWARD_PID" 2>/dev/null || true; fi
    if [ -n "$FOLLOW_PID" ]; then kill "$FOLLOW_PID" 2>/dev/null || true; fi
    # An add still waiting for a build that the run gave up on.
    stop_background

    if [ "$do_cleanup" = 0 ]; then
        echo "=== Leaving the cluster up (exit $exit_code) ==="
        echo "    Logs:       $LOG_FILE"
        echo "    Cluster:    export KUBECONFIG=$KUBECONFIG"
        echo "                kubectl -n aurcache logs deploy/aurcache-kube-worker"
        echo "                kubectl -n aurcache port-forward svc/aurcache 8080"
        echo "    Tear down:  $KIND delete cluster --name $CLUSTER"
        echo "                docker run --rm --name $CLUSTER-cleanup -v $SCRATCH:/d --entrypoint rm $NODE_IMAGE -rf /d/containerd /d/kubelet /d/local-path"
        echo "                rm -rf $SCRATCH"
        [ "${E2E_SKIP_BUILD:-0}" = 1 ] || echo "                docker rmi $SERVER_IMAGE $WORKER_IMAGE $GITSSH_IMAGE"
        return
    fi

    log "=== Cleaning up ==="
    if [ "$CLUSTER_CREATED" = 1 ]; then
        "$KIND" delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    fi
    # The node wrote as root under the scratch directory.
    if [ -d "$SCRATCH" ]; then
        docker run --rm --name "$CLUSTER-cleanup" -v "$SCRATCH:/d" --entrypoint rm "$NODE_IMAGE" \
            -rf /d/containerd /d/kubelet /d/local-path >/dev/null 2>&1 || true
        rm -rf "$SCRATCH"
    fi
    if [ "${E2E_SKIP_BUILD:-0}" != 1 ]; then
        docker rmi "$SERVER_IMAGE" "$WORKER_IMAGE" "$GITSSH_IMAGE" >/dev/null 2>&1 || true
    fi
}

# =============================================================================
# Main
# =============================================================================

# Every leg, in order. Some build on what an earlier one left: `repository`
# installs the package `quota` builds, `restart` and `stop` build `cancel`'s,
# `stuck` and `userns` `repository`'s.
LEGS=(enrollment policy quota repository ssh cancel restart stop stuck big userns open-policy)
if [ -n "${E2E_LEGS:-}" ]; then read -r -a LEGS <<<"$E2E_LEGS"; fi

RUN_STARTED=$SECONDS
preflight
trap cleanup EXIT
announce_log
build_cli
if [ "${REUSE:-0}" = 1 ]; then
    log "=== Reusing the cluster $CLUSTER (REUSE=1; nothing rebuilt or deployed) ==="
    CLUSTER_CREATED=1
else
    build_images
    create_cluster
    load_images
    deploy
fi
wait_for_service
for leg in "${LEGS[@]}"; do
    case $leg in
        enrollment) check_enrollment ;;
        policy) check_network_policy ;;
        quota) check_quota_then_build ;;
        repository) check_repository ;;
        ssh) check_ssh_key ;;
        cancel) check_cancel ;;
        restart) check_restart ;;
        stop) check_stop ;;
        stuck) check_stuck ;;
        big) check_big ;;
        userns) check_user_namespaces ;;
        open-policy) check_open_policy ;;
        *) fail "no leg named '$leg'" ;;
    esac
done

log "=== Kubernetes end-to-end test complete in $((SECONDS - RUN_STARTED))s ==="
for line in "${RESULTS[@]}"; do echo "    - $line"; done
