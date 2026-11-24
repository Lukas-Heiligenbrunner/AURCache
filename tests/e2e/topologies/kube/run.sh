#!/usr/bin/env bash
# End-to-end test for the Kubernetes worker, in a throwaway kind cluster.
#
# Builds the server and aurcache-worker-kube images from this tree, loads them
# into a new kind cluster, deploys the server from kubernetes/server/ and the
# worker from kubernetes/kube-worker/ as they ship (the kustomization beside
# this script adds what a test needs), and drives the server with the CLI.
# The legs, by the names that E2E_LEGS takes:
#
#   1. enrollment: the worker enrolls through the `aurcache-enrollment`
#      Secret, shows as `kubernetes`, takes the build agent from its own
#      image, its ServiceAccount cannot read pods in the server's namespace,
#      and its pod passes `restricted`;
#   2. policy: a pod labelled and confined like a build, in the builds'
#      namespace, reaches the server's repository and not its worker port,
#      which the same pod reaches from the test's own namespace;
#   3. build: <package> builds, with its output in the build log;
#   4. repository: a package builds against a dependency that only
#      AURCache's repository has;
#   5. ssh: a package fetches a source over git+ssh with the dispatcher's SSH
#      key, also from build();
#   6. restart: the worker is killed while a build runs; the new process
#      deletes the build pod, and the server ends the build and queues a new
#      one, which is cancelled during its build() and whose pod then goes;
#   7. stop: the worker's pod is deleted while a build runs; it stops within
#      its grace period, having deleted the build pod, and the server queues
#      a new build, which is cancelled likewise;
#   8. stuck: a build whose image cannot be pulled is handed back at its
#      start deadline as the worker's failure, so the server queues it again;
#   9. big: a package of 2 GiB builds and is published, and the run fails if
#      the worker container's peak memory reached 64 MiB;
#  10. live: aurcache-worker-kube's `#[ignore]`d tests/live.rs against this
#      cluster, as the dispatcher's own ServiceAccount.
#
#   tests/e2e/topologies/kube/run.sh [package] [timeout]
#
# package defaults to `hello` (from the AUR), timeout to 900 seconds a build.
#
# Needs kind, kubectl, docker, jq, curl, cargo, python3 and ssh-keygen, and
# the packaging submodules (`git submodule update --init`) for the image
# builds.
#
# Environment:
#   AURCACHE_TEST_PREFIX  prefix of the cluster, the images and the scratch
#                         directory (default aurcache-test)
#   KIND, KUBECTL         the binaries (default: from PATH)
#   E2E_SCRATCH           where the node keeps the pods' and the server's
#                         volumes: several GB with the 2 GiB package; default
#                         $TMPDIR/<prefix>-kube-e2e. `kind load` stages images
#                         in $TMPDIR
#   E2E_BUILDER_IMAGE     the builder image (default
#                         ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest),
#                         loaded from the local Docker when it is there,
#                         pulled by the node otherwise
#   E2E_SKIP_BUILD=1      use the images already built under the prefix
#   E2E_BIG=0             skip the 2 GiB package
#   E2E_MAX_WORKER_MEMORY the worker's peak memory at which the run fails, in bytes
#                         (default 64 MiB; the container's limit is 256 MiB)
#   KEEP=1                leave everything up, pass or fail, to look into it
#   E2E_LEGS              which legs to run, space-separated, by the names
#                         above (default: all, in order)
#   CLEANUP               as for the other e2e suites: 1 always tears down, 0
#                         never; unset, a failed run is left up
set -euo pipefail

PACKAGE="${1:-hello}"
BUILD_TIMEOUT="${2:-900}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
REPO_ROOT="$PROJECT_DIR"

PREFIX="${AURCACHE_TEST_PREFIX:-aurcache-test}"
CLUSTER="$PREFIX-kube-e2e"
NODE="$CLUSTER-control-plane"
KIND="${KIND:-kind}"
KUBECTL="${KUBECTL:-kubectl}"
# The node image released with kind v0.33.0, pinned by digest.
NODE_IMAGE="kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5"
SCRATCH="${E2E_SCRATCH:-${TMPDIR:-/tmp}/$CLUSTER}"
REGISTRY="localhost/$PREFIX"
TAG=kube-e2e
SERVER_IMAGE="$REGISTRY/aurcache-server:$TAG"
WORKER_IMAGE="$REGISTRY/aurcache-worker-kube:$TAG"
GITSSH_IMAGE="$REGISTRY/aurcache-gitssh:$TAG"
BUILDER_IMAGE="${E2E_BUILDER_IMAGE:-ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest}"
MAX_WORKER_MEMORY="${E2E_MAX_WORKER_MEMORY:-$((64 * 1024 * 1024))}"
# How long a Deployment gets to roll out, on a machine doing other things.
ROLLOUT=600s
GIT_URL="git://kube-e2e-git.kube-e2e.svc"
[ "${KEEP:-0}" = 1 ] && CLEANUP=0

export KUBECONFIG="$SCRATCH/kubeconfig"
export AURCACHE_TOKEN="${AURCACHE_TOKEN:-}"
# A port that refuses, until start_forward sets the real one. Unset or empty,
# the CLI would use the developer's own configuration and reach their instance.
export AURCACHE_URL="http://127.0.0.1:9/api"

# shellcheck source=scripts/lib/common.sh
. "$PROJECT_DIR/scripts/lib/common.sh"
# The compose-based harness's helpers: logging, `pkg_add`, the build lookups.
# What it does with compose is replaced below.
COMPOSE_ARGS=()
E2E_SERVICES=""
# shellcheck source=scripts/lib/e2e.sh
. "$PROJECT_DIR/scripts/lib/e2e.sh"

FORWARD_PID=
FOLLOW_PID=
CLUSTER_CREATED=0
RESULTS=()

k() { "$KUBECTL" "$@"; }

# What a leg found, for the summary at the end.
result() {
    RESULTS+=("$*")
    log "    => $*"
}

fail() {
    log "ERROR: $*"
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
# The worker
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

dispatcher_said() { dispatcher_log "${2:-}" | grep -qE -- "$1"; }

# Whether the worker logged no line matching `$1` since `$2`. A log that
# cannot be read says nothing either way, and fails the run.
dispatcher_silent() {
    local text
    text=$(dispatcher_log "${2:-}") || fail "the worker's log could not be read"
    ! grep -qE -- "$1" <<<"$text"
}

# Wait for the worker to log `$1`, since `$2`, for `$3` seconds.
expect_said() {
    local pattern=$1 since=${2:-} timeout=${3:-120}
    wait_for "$timeout" "the worker to log /$pattern/" dispatcher_said "$pattern" "$since" \
        || fail "the worker never logged /$pattern/"
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

# Kill the worker's process outright, as a crash would, and wait for the
# kubelet to start it again in the same pod.
crash_dispatcher() {
    local pod cid before
    pod=$(dispatcher_pod)
    before=$(dispatcher_restarts)
    cid=$(k -n aurcache get pod "$pod" -o jsonpath='{.status.containerStatuses[0].containerID}')
    # crictl gives the runtime two seconds to confirm unless told otherwise,
    # which a busy node does not always manage; the restart is what counts.
    docker exec "$NODE" crictl --timeout 30s stop --timeout 0 "${cid#containerd://}" >/dev/null \
        || log "    crictl did not confirm the kill; waiting for the restart all the same"
    wait_for 120 "the worker to start again" dispatcher_running_after "$before" \
        || fail "the worker did not come back after being killed"
}

worker_json() {
    aurcache_cli --format json worker list 2>/dev/null \
        | jq -c '[.[] | select(.kind == "kubernetes")] | first // empty'
}
worker_approved() { [ "$(worker_json | jq -r '.status // empty')" = approved ]; }

# Change the worker's environment with `kubectl set env` arguments `$@`, and
# wait for it to be back.
reconfigure_worker() {
    k -n aurcache set env deploy/aurcache-kube-worker "$@" >/dev/null
    k -n aurcache rollout status deploy/aurcache-kube-worker --timeout="$ROLLOUT" \
        || fail "the worker did not start again"
    wait_for 120 "the worker to be approved again" worker_approved || fail "the worker did not come back"
}

# =============================================================================
# Builds and pods
# =============================================================================

build_json() { aurcache_cli --format json builds get "$1" 2>/dev/null; }
build_status() { build_json "$1" | jq -r '.status'; }
build_output() { aurcache_cli builds output "$1" 2>/dev/null; }
build_said() { build_output "$1" | grep -qF -- "$2"; }
last_line() { build_output "$1" | grep -v '^\s*$' | tail -n 1; }

# BuildState's numbers, as the CLI's JSON gives them.
SUCCESSFUL=1
FAILED=2
build_over() {
    local status
    status=$(build_status "$1")
    [ "$status" = "$SUCCESSFUL" ] || [ "$status" = "$FAILED" ]
}

# Whether package `$1` has a build newer than `$2`.
ref_moved() { [ "$(latest_build_ref "$1")" != "$2" ]; }

# The build pods, as `name uid build-id` lines.
build_pods() {
    k -n aurcache-builds get pods -l aurcache.io/build-id -o json \
        | jq -r '.items[] | "\(.metadata.name) \(.metadata.uid) \(.metadata.labels["aurcache.io/build-id"])"'
}
# Both false when the pods cannot be listed, so that a wait goes on.
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

# The pod that build `$1` runs in, as its log names it; empty before it has one.
pod_of() { build_output "$1" | sed -n 's/^\[worker\] created pod //p' | tail -n 1; }

# Build package `$1` from the git daemon, and wait at most `$2` seconds
# (default BUILD_TIMEOUT) for it to succeed.
build_from_git() {
    local pkg=$1 timeout=${2:-$BUILD_TIMEOUT}
    pkg_add "$GIT_URL/$pkg.git" --ref master --wait --wait-timeout "$timeout" --wait-stall-after 600 \
        | sed 's/^/    /' || { CURRENT_BUILD_REF=$(latest_build_ref "$pkg"); fail "$pkg did not build"; }
    CURRENT_BUILD_REF=$(latest_build_ref "$pkg")
}

# The newest build of package `$1` once it is not `$2`.
new_build_ref() {
    wait_for 60 "a new build of $1" ref_moved "$1" "$2" || return 1
    latest_build_ref "$1"
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
    for tool in "$KIND" "$KUBECTL" docker jq curl cargo python3 ssh-keygen; do
        command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: '$tool' is required but not found"; lacking=1; }
    done
    docker info >/dev/null 2>&1 || { echo "ERROR: cannot talk to the Docker daemon"; lacking=1; }
    [ "$lacking" -eq 0 ] || exit 1
    if [ "${E2E_SKIP_BUILD:-0}" != 1 ]; then
        require_packaging_submodules aurcache-sandbox aurcache-worker aurcache-server \
            aurcache-worker-docker
    fi
    if "$KIND" get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
        echo "ERROR: a kind cluster named $CLUSTER exists already (a run left up?)."
        echo "       Delete it with: $KIND delete cluster --name $CLUSTER"
        exit 1
    fi
    if [ -d "$SCRATCH" ] && [ -n "$(ls -A "$SCRATCH")" ]; then
        echo "ERROR: $SCRATCH holds what an earlier run left. Remove it with:"
        echo "       docker run --rm --name $CLUSTER-cleanup -v $SCRATCH:/d --entrypoint rm $NODE_IMAGE -rf /d/kubelet /d/local-path"
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
    mkdir -p "$SCRATCH"/{kubelet,local-path}
    # The pods' volumes and the server's go to the scratch directory. Images
    # stay in the node's own storage, which holds the control plane's
    # preloaded images.
    cat >"$SCRATCH/cluster.yaml" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    image: $NODE_IMAGE
    extraMounts:
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
        # One platform only: Docker's containerd store saves a pulled image
        # with an index of every platform but the blobs of one, which the
        # node's import refuses.
        docker save --platform linux/amd64 -o "$SCRATCH/builder.tar" "$BUILDER_IMAGE"
        "$KIND" load image-archive --name "$CLUSTER" "$SCRATCH/builder.tar"
        rm -f "$SCRATCH/builder.tar"
    else
        log "    $BUILDER_IMAGE is not in the local Docker; the node pulls it"
    fi
}

# The run's own kustomization over the fixture beside this script: what
# differs per run.
write_overlay() {
    local dir="$SCRATCH/overlay" base token pkgbuild packages=''
    mkdir -p "$dir/packages" "$dir/ssh"
    base=$(realpath --relative-to="$dir" "$SCRIPT_DIR")
    token=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
    for pkgbuild in "$SCRIPT_DIR"/packages/*/PKGBUILD; do
        local name
        name=$(basename "$(dirname "$pkgbuild")")
        cp "$pkgbuild" "$dir/packages/$name.PKGBUILD"
        packages+="      - packages/$name.PKGBUILD"$'\n'
    done
    # The dispatcher's SSH key, and the git server's host key, so that its
    # known_hosts can name it before the server runs.
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
$packages    options:
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
    log "=== Deploying the server, the git servers and the Kubernetes worker ==="
    write_overlay
    k apply -k "$SCRATCH/overlay" >/dev/null
    local deployment
    for deployment in aurcache/aurcache kube-e2e/kube-e2e-gitssh kube-e2e/kube-e2e-git \
        aurcache/aurcache-kube-worker; do
        k -n "${deployment%/*}" rollout status "deploy/${deployment#*/}" --timeout="$ROLLOUT" \
            || fail "$deployment did not become ready"
    done
}

# The API through a port-forward to the server's Service, on a free port.
# Started once, in the main shell, where cleanup can stop it.
start_forward() {
    local out="$SCRATCH/port-forward.log" port='' i
    # kubectl itself, not `k`, so that FORWARD_PID is kubectl's own.
    "$KUBECTL" -n aurcache port-forward svc/aurcache :8080 >"$out" 2>&1 &
    FORWARD_PID=$!
    for i in $(seq 50); do
        port=$(sed -n 's/^Forwarding from 127\.0\.0\.1:\([0-9]*\) .*/\1/p' "$out" | head -n 1)
        [ -n "$port" ] && break
        sleep 0.2
    done
    [ -n "$port" ] || fail "the port-forward to the server did not start: $(cat "$out")"
    export AURCACHE_URL="http://127.0.0.1:$port/api"
}

wait_for_service() {
    log "=== Waiting for the API ==="
    start_forward
    wait_for 120 "the API" aurcache_cli health >/dev/null || fail "the API did not answer"
}

# =============================================================================
# The legs
# =============================================================================

check_enrollment() {
    log "=== 1. The worker enrolls and starts ==="
    wait_for 180 "the Kubernetes worker to be approved" worker_approved \
        || fail "no approved worker of kind kubernetes (the enrollment Secret should approve it)"
    log "    worker: $(worker_json | jq -r '"\(.name), \(.kind), \(.status), arches \(.arches | join(","))"')"
    result "enrolled through the Secret, approved, kind kubernetes"

    expect_said "Build pods go to namespace aurcache-builds and take the build agent from $WORKER_IMAGE" "" 60
    expect_said "Builds fetch their sources with the SSH key" "" 10
    # `can-i` prints its answer; an error asking is neither.
    local may
    may=$(k auth can-i get pods -n aurcache \
        --as=system:serviceaccount:aurcache:aurcache-kube-worker 2>&1 || true)
    [ "$may" = no ] \
        || fail "the worker's ServiceAccount should not read pods in its own namespace; can-i said: $may"

    # The worker's own pod, as the API server would admit it to a namespace
    # that enforces `restricted`.
    local admitted
    k create namespace kube-e2e-restricted --dry-run=client -o yaml | k apply -f - >/dev/null
    k label --overwrite namespace kube-e2e-restricted \
        pod-security.kubernetes.io/enforce=restricted >/dev/null
    admitted=$(k -n aurcache get pod "$(dispatcher_pod)" -o json \
        | jq '{apiVersion, kind, metadata: {name: "restricted-check"},
               spec: (.spec | del(.nodeName, .serviceAccountName, .serviceAccount))}' \
        | k -n kube-e2e-restricted create --dry-run=server -f - 2>&1) || admitted="refused: $admitted"
    k delete namespace kube-e2e-restricted --wait=false >/dev/null 2>&1 || true
    [[ $admitted != refused:* ]] || fail "the worker's pod is not one \`restricted\` admits: $admitted"
    result "the worker names its agent image and SSH key, cannot read pods in the server's namespace, and passes \`restricted\`"
    dispatcher_silent "Deleted [0-9]+ build pod" || fail "a first start found build pods to delete"
}

# The server's worker port, by its Service and by its pod's own address, and
# its repository, which builds may reach.
WORKER_PORT_SERVICE=aurcache.aurcache.svc:8083
REPOSITORY=aurcache.aurcache.svc:8081

# Starts a pod named `$2` in namespace `$1`, labelled and confined like a
# build, and sets REACHED to one `<host:port> open|closed` line per target:
# `$3`, the worker port's Service and the repository. kindnet applies the
# policy a few seconds after a pod starts, so the pod retries for up to a
# minute until `$3` reads `$4`.
reach_from() {
    local ns=$1 name=$2 target=$3 want=$4
    # For the pod's shell to expand, not this one.
    # shellcheck disable=SC2016
    local script='
try() { if timeout 5 bash -c "exec 3<>/dev/tcp/${1%:*}/${1##*:}" 2>/dev/null; then echo "$1 open"; else echo "$1 closed"; fi; }
for i in $(seq 12); do
  out=$(for t in "$@"; do try "$t"; done)
  case "$out" in *"$WANT"*) break ;; esac
  sleep 5
done
echo "$out"'
    local out
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
    - name: reach
      image: $WORKER_IMAGE
      imagePullPolicy: IfNotPresent
      env:
        - name: WANT
          value: "$target $want"
      command: [bash, -c, $(jq -Rs . <<<"$script"), reach, "$target", "$WORKER_PORT_SERVICE", "$REPOSITORY"]
      securityContext:
        allowPrivilegeEscalation: false
        capabilities:
          drop: [ALL]
EOF
    ) || fail "the pod labelled like a build failed in $ns: $out"
    REACHED=$(grep -E ' (open|closed)$' <<<"$out")
    printf '%s\n' "$REACHED" | sed 's/^/    | /'
}

check_network_policy() {
    log "=== 2. The builds' NetworkPolicy ==="
    local ip target
    ip=$(k -n aurcache get pods -l app.kubernetes.io/component=server \
        -o jsonpath='{.items[0].status.podIP}' 2>/dev/null || true)
    [ -n "$ip" ] || fail "the server's pod has no address"
    ip+=:8083
    # The control: the same pod, in a namespace that no policy selects.
    reach_from kube-e2e kube-e2e-control "$ip" open
    for target in "$ip" "$WORKER_PORT_SERVICE" "$REPOSITORY"; do
        grep -qx "$target open" <<<"$REACHED" || fail "the control pod in kube-e2e could not reach $target"
    done
    reach_from aurcache-builds kube-e2e-policy "$ip" closed
    grep -qx "$ip closed" <<<"$REACHED" \
        || fail "a pod labelled like a build reached the server pod's worker port"
    grep -qx "$WORKER_PORT_SERVICE closed" <<<"$REACHED" \
        || fail "a pod labelled like a build reached the server's worker port"
    grep -qx "$REPOSITORY open" <<<"$REACHED" \
        || fail "a pod labelled like a build could not reach the repository"
    result "a pod labelled like a build reaches the repository, and the worker port only from outside the builds' namespace"
}

check_build() {
    log "=== 3. $PACKAGE builds ==="
    local ref output line
    pkg_add "$PACKAGE" --platform x86_64 \
        --wait --wait-timeout "$BUILD_TIMEOUT" --wait-stall-after 600 \
        | sed 's/^/    /' || { CURRENT_BUILD_REF=$(latest_build_ref "$PACKAGE"); fail "$PACKAGE did not build"; }
    ref=$(latest_build_ref "$PACKAGE")
    CURRENT_BUILD_REF=$ref
    output=$(build_output "$ref")
    for line in "[worker] created pod" "[worker] handing the job to the pod" "==> Finished making"; do
        grep -qF -- "$line" <<<"$output" || fail "the log of $ref has no '$line'"
    done
    wait_for 60 "the build pod of $ref to go" no_build_pods || fail "the pod of $ref was left behind"
    result "$ref built, with its output in its log, and its pod went"
}

check_repository() {
    log "=== 4. A dependency from AURCache's own repository ==="
    build_from_git kube-e2e-dep
    build_from_git kube-e2e-app
    local ref=$CURRENT_BUILD_REF
    build_said "$ref" "kube-e2e-app: found kube-e2e-dep's marker" \
        || fail "the build of $ref did not find its dependency installed"
    result "$ref built against kube-e2e-dep installed from AURCache's repository"
}

check_ssh_key() {
    log "=== 5. A source over git+ssh with the dispatcher's key ==="
    build_from_git kube-e2e-ssh
    local ref=$CURRENT_BUILD_REF output
    output=$(build_output "$ref")
    grep -E "SSH key|kube-e2e-ssh:" <<<"$output" | sed 's/^/    | /'
    grep -qF "kube-e2e-ssh: payload aurcache-ssh-credential-reached-the-chroot" <<<"$output" \
        || fail "$ref did not fetch its git+ssh source"
    grep -qF "kube-e2e-ssh: reached the server from build()" <<<"$output" \
        || fail "$ref did not reach the SSH server from build()"
    result "$ref fetched over git+ssh through the dispatcher, from build() too"
}

# Start a build of kube-e2e-slow and wait for its build() to run: SLOW_REF is
# the build, SLOW_POD and SLOW_UID its pod.
start_slow() {
    local before
    before=$(latest_build_ref kube-e2e-slow)
    # Added once; after that an add queues nothing, so its last build is retried.
    if [ -z "$before" ]; then
        pkg_add "$GIT_URL/kube-e2e-slow.git" --ref master
    else
        aurcache_cli builds retry "$before"
    fi | sed 's/^/    /' || fail "starting a build of kube-e2e-slow failed"
    SLOW_REF=$(new_build_ref kube-e2e-slow "$before") || fail "no build of kube-e2e-slow was queued"
    CURRENT_BUILD_REF=$SLOW_REF
    wait_for "$BUILD_TIMEOUT" "$SLOW_REF to be building" build_said "$SLOW_REF" "kube-e2e-slow: sleeping" \
        || fail "$SLOW_REF never started building"
    SLOW_POD=$(pod_of "$SLOW_REF")
    SLOW_UID=$(k -n aurcache-builds get pod "$SLOW_POD" -o jsonpath='{.metadata.uid}') \
        || fail "the pod of $SLOW_REF, ${SLOW_POD:-unnamed in its log}, is not there"
    log "    $SLOW_REF runs in $SLOW_POD ($SLOW_UID)"
}

# The server ended SLOW_REF, whose pod is gone, and queued a new build of
# the package; that build is cancelled once its build() runs, and its pod
# must go within a minute.
expect_requeued() {
    wait_for 120 "$SLOW_REF to end" build_over "$SLOW_REF" || fail "$SLOW_REF was not ended"
    log "    $SLOW_REF: $(last_line "$SLOW_REF")"
    local again started
    again=$(new_build_ref kube-e2e-slow "$SLOW_REF") || fail "the server did not queue kube-e2e-slow again"
    CURRENT_BUILD_REF=$again
    wait_for "$BUILD_TIMEOUT" "$again to be building" build_said "$again" "kube-e2e-slow: sleeping" \
        || fail "$again never started building"
    started=$SECONDS
    aurcache_cli builds cancel "$again" | sed 's/^/    /'
    wait_for 60 "the pod of $again to go" no_build_pods || fail "the pod of $again outlived its cancel by a minute"
    wait_for 60 "$again to end" build_over "$again" || fail "$again did not end after its cancel"
    result "the server ended $SLOW_REF and queued $again, which built in a new pod; cancelled, its pod went in $((SECONDS - started))s"
}

check_restart() {
    log "=== 6. The worker dies under a build ==="
    start_slow
    local since
    since=$(now)
    crash_dispatcher
    expect_said "Deleted 1 build pod\(s\) left by a previous run of this worker" "$since" 120
    wait_for 60 "pod $SLOW_UID to go" pod_uid_gone "$SLOW_UID" || fail "the old build pod survived the sweep"
    result "worker killed mid-build: the new process deleted $SLOW_POD"
    expect_requeued
}

# Whether no pod named `$1` is in the server's namespace any more.
dispatcher_pod_gone() { ! k -n aurcache get pod "$1" >/dev/null 2>&1; }

process_gone() { ! kill -0 "$1" 2>/dev/null; }

check_stop() {
    log "=== 7. The worker is stopped under a build ==="
    start_slow
    # The pod's log goes with the pod, so it is followed from before the
    # stop, once kubectl has written what is there so far.
    local old said took started
    old=$(dispatcher_pod)
    said="$SCRATCH/stopped-worker.log"
    : >"$said"
    "$KUBECTL" -n aurcache logs -f "$old" -c worker >"$said" 2>"$said.err" &
    FOLLOW_PID=$!
    wait_for 30 "the log of $old to be followed" test -s "$said" \
        || fail "the log of $old could not be followed: $(cat "$said.err")"
    started=$SECONDS
    k -n aurcache delete pod "$old" --wait=false >/dev/null
    # Its grace period is 60s, at the end of which a worker that ignored the
    # signal is killed.
    wait_for 54 "the worker's pod $old to stop" dispatcher_pod_gone "$old" \
        || fail "the worker's pod was still there 54s after it was deleted: it did not stop when told to"
    took=$((SECONDS - started))
    wait_for 30 "the log of $old to end" process_gone "$FOLLOW_PID" \
        || fail "following the log of $old did not end with its pod"
    wait "$FOLLOW_PID" 2>/dev/null || true
    FOLLOW_PID=
    sed -i 's/\x1b\[[0-9;]*m//g' "$said"
    grep -E "Received SIGTERM|deleted pod $SLOW_POD\b" "$said" | sed 's/^/    | /'
    grep -qE "deleted pod $SLOW_POD\b" "$said" || fail "the stopped worker did not log deleting $SLOW_POD"
    wait_for 30 "pod $SLOW_UID to go" pod_uid_gone "$SLOW_UID" || fail "the build pod outlived the worker that ran it"
    result "worker pod deleted mid-build: it stopped in ${took}s, and deleted $SLOW_POD itself"

    k -n aurcache rollout status deploy/aurcache-kube-worker --timeout="$ROLLOUT" \
        || fail "the worker did not start again"
    expect_requeued
    # The new worker's sweep came before its claim.
    dispatcher_silent "Deleted [0-9]+ build pod\(s\) left by a previous run" \
        || fail "the new worker found the build pod its stopped predecessor should have deleted"
}

check_stuck() {
    log "=== 8. A build that cannot start ==="
    local before ref want again
    # The pull fails, so the build is not running by the start deadline.
    reconfigure_worker WORKER_KUBE_BUILDER_IMAGE=localhost/kube-e2e/missing:none \
        WORKER_KUBE_SCHEDULE_TIMEOUT=30s
    before=$(latest_build_ref kube-e2e-dep)
    aurcache_cli builds retry "$before" | sed 's/^/    /'
    ref=$(new_build_ref kube-e2e-dep "$before") || fail "the retry queued nothing"
    CURRENT_BUILD_REF=$ref
    wait_for 150 "$ref to be handed back" build_over "$ref" || fail "$ref was not handed back within 150s"
    for want in "did not start within 30s" "ImagePull"; do
        build_said "$ref" "$want" || fail "the log of $ref does not say '$want': $(last_line "$ref")"
    done
    # The server queues a build again only when the worker, not the package,
    # is what failed.
    again=$(new_build_ref kube-e2e-dep "$ref") || fail "the server did not queue kube-e2e-dep again after $ref"
    result "$ref was handed back at its start deadline, and the server queued $again"
    CURRENT_BUILD_REF=$again
    aurcache_cli builds cancel "$again" | sed 's/^/    /'
    reconfigure_worker WORKER_KUBE_BUILDER_IMAGE="$BUILDER_IMAGE" WORKER_KUBE_SCHEDULE_TIMEOUT-
    wait_for 60 "the pod of $again to go" no_build_pods || fail "the pod of $again outlived its cancel"
}

check_big() {
    log "=== 9. A package of 2 GiB ==="
    if [ "${E2E_BIG:-1}" = 0 ]; then
        result "skipped: the 2 GiB package (E2E_BIG=0)"
        return
    fi
    local restarts size limit peak
    restarts=$(dispatcher_restarts)
    build_from_git kube-e2e-big 3600
    size=$(build_json "$CURRENT_BUILD_REF" | jq -r '.size')
    ((size > 2 * 1024 * 1024 * 1024)) || fail "$CURRENT_BUILD_REF published only $size bytes"
    result "$CURRENT_BUILD_REF: $((size / 1024 / 1024)) MiB published"

    # The container's own cgroup, as its memory limit shows.
    limit=$(k -n aurcache exec deploy/aurcache-kube-worker -c worker -- cat /sys/fs/cgroup/memory.max)
    peak=$(k -n aurcache exec deploy/aurcache-kube-worker -c worker -- cat /sys/fs/cgroup/memory.peak)
    [ "$limit" = "$((256 * 1024 * 1024))" ] \
        || fail "the worker's cgroup has limit $limit, not the manifest's 256Mi: the wrong cgroup?"
    [ "$(dispatcher_restarts)" = "$restarts" ] || fail "the worker restarted during the relay"
    ((peak < MAX_WORKER_MEMORY)) || fail "the worker's peak memory, $peak bytes, reached $MAX_WORKER_MEMORY"
    result "the worker container's peak memory: $((peak / 1024 / 1024)) MiB (limit for the run $((MAX_WORKER_MEMORY / 1024 / 1024)) MiB)"
}

# A kubeconfig at `$1` for this cluster, with the credentials of the
# dispatcher's own ServiceAccount rather than an administrator's.
write_worker_kubeconfig() {
    local out=$1 server ca token
    server=$(k config view --raw --minify -o jsonpath='{.clusters[0].cluster.server}')
    ca=$(k config view --raw --minify -o jsonpath='{.clusters[0].cluster.certificate-authority-data}')
    token=$(k -n aurcache create token aurcache-kube-worker --duration=2h)
    (
        umask 077
        printf '%s\n' \
            'apiVersion: v1' 'kind: Config' \
            'clusters:' '  - name: kind' '    cluster:' \
            "      server: $server" "      certificate-authority-data: $ca" \
            'users:' '  - name: aurcache-kube-worker' '    user:' "      token: $token" \
            'contexts:' '  - name: worker' '    context:' '      cluster: kind' \
            '      user: aurcache-kube-worker' '      namespace: aurcache-builds' \
            'current-context: worker' >"$out"
    )
}

# The crate's tests that need a cluster, as the dispatcher's ServiceAccount:
# they show that its Role is enough.
check_live_tests() {
    log "=== 10. aurcache-worker-kube's tests against the cluster ==="
    local config="$SCRATCH/kube-worker.kubeconfig"
    # Built before the token is made, which a slow build could outlast.
    (cd "$PROJECT_DIR/backend" && cargo test -q -p aurcache-worker-kube --test live --no-run) \
        || fail "aurcache-worker-kube's tests did not build"
    write_worker_kubeconfig "$config"
    (cd "$PROJECT_DIR/backend" && KUBECONFIG=$config AURCACHE_KUBE_TESTS=1 AURCACHE_KUBE_TEST_IMAGE=$WORKER_IMAGE \
        cargo test -q -p aurcache-worker-kube --test live -- --ignored --test-threads=1 --nocapture) \
        || { rm -f "$config"; fail "aurcache-worker-kube's tests against the cluster failed"; }
    rm -f "$config"
    result "aurcache-worker-kube's #[ignore]d tests passed against the cluster, as its ServiceAccount"
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

    if [ -n "$FORWARD_PID" ] && ! kill -0 "$FORWARD_PID" 2>/dev/null; then
        echo "--- the port-forward to the server died ---"
        cat "$SCRATCH/port-forward.log" 2>/dev/null || true
    fi
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
    local exit_code=$?
    # Whatever ended the run: `fail`, or a command that `set -e` stopped at.
    [ "$exit_code" -eq 0 ] || dump_logs_on_failure
    if [ -n "$FORWARD_PID" ]; then kill "$FORWARD_PID" 2>/dev/null || true; fi
    if [ -n "$FOLLOW_PID" ]; then kill "$FOLLOW_PID" 2>/dev/null || true; fi

    if ! cleanup_wanted "$exit_code"; then
        echo "=== Leaving the cluster up (exit $exit_code) ==="
        echo "    Logs:       $LOG_FILE"
        echo "    Cluster:    export KUBECONFIG=$KUBECONFIG"
        echo "                kubectl -n aurcache logs deploy/aurcache-kube-worker"
        echo "                kubectl -n aurcache port-forward svc/aurcache 8080"
        echo "    Tear down:  $KIND delete cluster --name $CLUSTER"
        echo "                docker run --rm --name $CLUSTER-cleanup -v $SCRATCH:/d --entrypoint rm $NODE_IMAGE -rf /d/kubelet /d/local-path"
        echo "                rm -rf $SCRATCH"
        return
    fi

    log "=== Cleaning up ==="
    if [ "$CLUSTER_CREATED" = 1 ]; then
        "$KIND" delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    fi
    # The node wrote as root under the scratch directory.
    if [ -d "$SCRATCH" ]; then
        docker run --rm --name "$CLUSTER-cleanup" -v "$SCRATCH:/d" --entrypoint rm "$NODE_IMAGE" \
            -rf /d/kubelet /d/local-path >/dev/null 2>&1 || true
        rm -rf "$SCRATCH"
    fi
}

# =============================================================================
# Main
# =============================================================================

# Every leg, in order. `stuck` retries the dependency that `repository` built.
LEGS=(enrollment policy build repository ssh restart stop stuck big live)
if [ -n "${E2E_LEGS:-}" ]; then read -r -a LEGS <<<"$E2E_LEGS"; fi

RUN_STARTED=$SECONDS
preflight
trap cleanup EXIT
announce_log
build_cli
build_images
create_cluster
load_images
deploy
wait_for_service
for leg in "${LEGS[@]}"; do
    case $leg in
        enrollment) check_enrollment ;;
        policy) check_network_policy ;;
        build) check_build ;;
        repository) check_repository ;;
        ssh) check_ssh_key ;;
        restart) check_restart ;;
        stop) check_stop ;;
        stuck) check_stuck ;;
        big) check_big ;;
        live) check_live_tests ;;
        *) fail "no leg named '$leg'" ;;
    esac
done

log "=== Kubernetes end-to-end test complete in $((SECONDS - RUN_STARTED))s ==="
for line in "${RESULTS[@]}"; do echo "    - $line"; done
