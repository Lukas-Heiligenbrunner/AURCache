#!/usr/bin/env bash
# Build the multi-architecture AURCache images.
#
# What the publish workflows run, and runnable by hand.
#
#   server       aurcache-server: the backend alone -- what a split deployment
#                runs.
#   worker       aurcache-worker: a build worker, for a split deployment.
#   hybrid       aurcache: the hybrid (server + embedded worker) compatibility
#                image.
#   builder      aurcache-builder: the container the hybrid's legacy builder
#                spawns per build.
#   demo-worker  aurcache-demo-worker: the dummy worker for demo instances,
#                synthetic packages and no build. Debian, like the server.
#
# The worker and hybrid images are two targets of docker/arch.Dockerfile. They
# install AURCache as Arch packages, which its packager stage cross-compiles on
# the build host, once for both; only their runtime stages are
# per-architecture, so an emulated build spends its time on `pacman -U` rather
# than on cargo. The server image is Debian and cross-compiles through
# build-rust.sh. The demo-worker image is Debian too but builds natively per
# platform, which under emulation is slow rather than wrong (see
# docker/demo-worker.Dockerfile).
#
#   scripts/build-images.sh docker.example.com
#   scripts/build-images.sh --push --tag v0.5.0 --tag latest docker.example.com
#   scripts/build-images.sh --images worker --platforms linux/amd64 localhost:5000
#
# armv7 needs a cross toolchain that is not in Arch's repositories and takes
# the better part of an hour to build from the AUR. Point `--toolchain-repo` at
# a pacman repository that already has it -- an AURCache instance, for
# example -- and that becomes a few seconds. See packaging/build-cross-toolchain.sh.
#
# The worker and hybrid images *build* the AURCache packages as a stage of their
# own and then install them. `--packages-dir` keeps that work for the host too:
# the packager stage is replayed from the build cache into the directory, so a
# local `pacman -U` gets the same artifacts the images just installed, without
# `build-packages.sh` building them a second time.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname -- "$SCRIPT_DIR")"
readonly SCRIPT_DIR REPO_ROOT

readonly DEFAULT_PLATFORMS="linux/amd64,linux/arm64,linux/arm/v7"
readonly DEFAULT_BUILDER="aurcache-multiarch"

# shellcheck source=scripts/lib/common.sh
. "$SCRIPT_DIR/lib/common.sh"

usage() {
    header_help "${BASH_SOURCE[0]}"
    cat <<'EOF'
Options:
  -t, --tag TAG             Image tag; repeat for several (default: latest)
  -i, --images LIST         Comma-separated: server, worker, hybrid, builder,
                            demo-worker (default: server, worker, hybrid)
  -p, --platforms LIST      Target platforms (default: linux/amd64,linux/arm64,linux/arm/v7)
      --push                Push to the registry; without it the images are
                            built and discarded (see the note below)
      --toolchain-repo URL  pacman repository holding the prebuilt armv7 cross
                            toolchain, e.g. 'http://host:8081/$arch'
      --toolchain-repo-name NAME
                            That repository's section name, which decides the
                            database file pacman fetches (default: repo)
      --builder NAME        buildx builder to use (default: aurcache-multiarch,
                            created on demand)
      --cache gha           Restore and save the build cache in GitHub Actions'
                            cache backend, one scope per image. The step needs
                            the runtime's token in its environment (see
                            docker-worker-publish.yml).
      --packages-dir DIR    Also write the AURCache packages the worker/hybrid
                            images built into DIR, for a native install. The
                            packager stage is replayed from the build cache, so
                            this is an output, not a second build. Host arch
                            (x86_64) only.
  -h, --help                This message

Without --push, what happens to the result depends on the daemon's image
store. The classic store holds one architecture per tag, so a multi-platform
build is discarded and only the build cache survives -- still useful for
checking that every architecture builds. Docker's containerd image store holds
multi-arch images, and this script loads into it when the daemon has it
enabled. A single-platform build is always loaded.
EOF
}

tags=()
images=server,worker,hybrid
platforms=$DEFAULT_PLATFORMS
push=0
toolchain_repo=
toolchain_repo_name=
builder=$DEFAULT_BUILDER
packages_dir=
cache=
registry=

while (($#)); do
    case $1 in
        -t | --tag) tags+=("$2"); shift 2 ;;
        -i | --images) images=$2; shift 2 ;;
        -p | --platforms) platforms=$2; shift 2 ;;
        --push) push=1; shift ;;
        --toolchain-repo) toolchain_repo=$2; shift 2 ;;
        --toolchain-repo-name) toolchain_repo_name=$2; shift 2 ;;
        --builder) builder=$2; shift 2 ;;
        --packages-dir) packages_dir=$2; shift 2 ;;
        --cache) cache=$2; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
        *)
            [[ -n $registry ]] && { echo "unexpected argument: $1" >&2; exit 2; }
            registry=$1
            shift
            ;;
    esac
done

if [[ -z $registry ]]; then
    echo "error: no registry/namespace given" >&2
    usage >&2
    exit 2
fi
((${#tags[@]})) || tags=(latest)
if [[ -n $cache && $cache != gha ]]; then
    echo "error: unknown cache backend '$cache' (expected gha)" >&2
    exit 2
fi

# The image each name publishes as. `aurcache` rather than `aurcache-hybrid`
# because that is the name deployments predating the split already pull.
declare -A IMAGE_NAMES=(
    [server]=aurcache-server
    [worker]=aurcache-worker
    [hybrid]=aurcache
    [builder]=aurcache-builder
    [demo-worker]=aurcache-demo-worker
)
# The Dockerfile each is built from, and the stage, for the two that share one.
declare -A DOCKERFILES=(
    [server]=docker/server.Dockerfile
    [worker]=docker/arch.Dockerfile
    [hybrid]=docker/arch.Dockerfile
    [builder]=docker/builder.Dockerfile
    [demo-worker]=docker/demo-worker.Dockerfile
)
declare -A TARGETS=(
    [worker]=worker
    [hybrid]=hybrid
)

IFS=',' read -r -a selected <<<"$images"
packaged=0
for image in "${selected[@]}"; do
    if [[ -z ${IMAGE_NAMES[$image]:-} ]]; then
        echo "error: unknown image '$image' (expected server, worker, hybrid, builder or demo-worker)" >&2
        exit 2
    fi
    [[ -n ${TARGETS[$image]:-} ]] && packaged=1
done

# The default `docker` driver builds for one architecture only, and fails a
# multi-platform build with a message that does not say so; it cannot write
# `type=local` output (--packages-dir) or save a cache to GitHub's backend
# either. Any of those uses a docker-container builder, created rather than
# demanded, since one is as good as another.
use_container_builder=0
if [[ $platforms == *,* || -n $packages_dir || -n $cache ]]; then
    use_container_builder=1
fi
builder_args=()
if ((use_container_builder)); then
    if ! docker buildx inspect "$builder" >/dev/null 2>&1; then
        echo "==> creating buildx builder '$builder'"
        docker buildx create --name "$builder" --driver docker-container --bootstrap >/dev/null
    fi
    builder_args=(--builder "$builder")
fi

# Foreign runtime stages run under qemu-user through binfmt. Checked rather than
# assumed: without it the build fails partway with "exec format error", which
# does not point at the cause.
if [[ $platforms == *arm* ]] && [[ ! -e /proc/sys/fs/binfmt_misc/qemu-arm ]] &&
    [[ ! -e /proc/sys/fs/binfmt_misc/qemu-aarch64 ]]; then
    echo "warning: no qemu binfmt handler registered; ARM stages will fail." >&2
    echo "         register one with: docker run --privileged --rm tonistiigi/binfmt --install all" >&2
fi

# Where the result goes.
#
# The classic image store holds one architecture per tag, so a multi-platform
# build cannot be loaded into it -- buildx refuses, and the only options are
# pushing or discarding. Docker's containerd image store does hold multi-arch
# images, so when the daemon is using it the build can be loaded and run
# locally. Enable it with, in /etc/docker/daemon.json:
#
#   { "features": { "containerd-snapshotter": true } }
#
# and restart the daemon. Images in the classic store stop being *visible*
# while it is active (they are not deleted, and turning it off brings them
# back), which is worth knowing before switching on a machine with a lot of
# them.
containerd_store=0
if docker info --format '{{.DriverStatus}}' 2>/dev/null | grep -q 'io.containerd.snapshotter'; then
    containerd_store=1
fi

if ((push)); then
    output_args=(--push)
elif ((containerd_store)); then
    output_args=(--load)
elif [[ $platforms == *,* ]]; then
    output_args=(--output "type=cacheonly")
    echo "note: the daemon uses the classic image store, which cannot hold a"
    echo "      multi-platform image, so this build will be discarded. Enable"
    echo "      the containerd image store to keep it, or use --push."
else
    # One platform: the classic store can hold that.
    output_args=(--load)
fi

build_args=()
if [[ -n $toolchain_repo ]]; then
    build_args+=(--build-arg "AURCACHE_TOOLCHAIN_REPO=$toolchain_repo")
fi
if [[ -n $toolchain_repo_name ]]; then
    build_args+=(--build-arg "AURCACHE_TOOLCHAIN_REPO_NAME=$toolchain_repo_name")
fi
# What the image's binaries report as their version. `.git` never reaches the
# image (see .dockerignore), so the commit, the release tag when this tree is
# exactly one, and the dirty state go in explicitly rather than being probed
# inside (see aurcache-common's build script). Outside a checkout there is
# simply nothing to pass, and the binaries report their bare release.
#
# Every image but the builder consumes these (the server and the demo worker
# in their builder stages, the others in their packager stage).
version_args=()
if sha=$(git rev-parse HEAD 2>/dev/null); then
    version_args+=(--build-arg "LATEST_COMMIT_SHA=$sha")
    if git_tag=$(git describe --tags --exact-match HEAD 2>/dev/null); then
        version_args+=(--build-arg "AURCACHE_GIT_TAG=$git_tag")
    fi
    if [[ -n $(git status --porcelain --untracked-files=no 2>/dev/null) ]]; then
        version_args+=(--build-arg "AURCACHE_GIT_DIRTY=1")
    fi
fi
if [[ $platforms == *arm/v7* ]] && [[ -z $toolchain_repo ]]; then
    echo "note: building armv7 without --toolchain-repo; the cross toolchain is"
    echo "      built from the AUR and takes roughly an hour the first time."
fi

# The worker and hybrid images build these as Arch packages; the server image
# does not, but checking unconditionally keeps the message the same wherever it
# is hit.
require_packaging_submodules aurcache-sandbox aurcache-worker aurcache-server \
    aurcache-worker-docker

for image in "${selected[@]}"; do
    name="$registry/${IMAGE_NAMES[$image]}"
    tag_args=()
    for tag in "${tags[@]}"; do tag_args+=(--tag "$name:$tag"); done
    echo
    echo "==> $image -> $name:{$(IFS=,; echo "${tags[*]}")}  [$platforms]"

    image_args=()
    target=${TARGETS[$image]:-}
    [[ -n $target ]] && image_args+=(--target "$target")
    # buildkit warns about a build arg no stage consumes, so each image gets
    # only those it declares: the cross-toolchain args belong to the Arch
    # packager stage, and the builder image declares none at all.
    [[ -n $target ]] && image_args+=("${build_args[@]}")
    [[ $image != builder ]] && image_args+=("${version_args[@]}")
    # Scoped per image: several images build in one CI job and would
    # otherwise share a cache key, each overwriting the other's entry.
    if [[ -n $cache ]]; then
        image_args+=(--cache-from "type=$cache,scope=$image" \
            --cache-to "type=$cache,scope=$image,mode=max")
    fi

    docker buildx build "${builder_args[@]}" \
        --platform "$platforms" \
        --file "$REPO_ROOT/${DOCKERFILES[$image]}" \
        "${tag_args[@]}" \
        "${image_args[@]}" \
        "${output_args[@]}" \
        "$REPO_ROOT"
done

# The packager stage just built the AURCache packages, which the images
# install without keeping. Give the host its own copy: a second build
# targeting the export-pkgs stage replays that stage from the build cache the
# image build just populated and copies the archives out with `type=local`
# output.
if [[ -n $packages_dir ]]; then
    echo
    if ((packaged)); then
        echo "==> exporting built packages to $packages_dir (x86_64, host arch)"
        mkdir -p "$packages_dir"
        docker buildx build "${builder_args[@]}" \
            --platform linux/amd64 \
            --file "$REPO_ROOT/docker/arch.Dockerfile" \
            --target export-pkgs \
            "${version_args[@]}" \
            --output "type=local,dest=$packages_dir" \
            "$REPO_ROOT"
    else
        echo "note: --packages-dir given but no worker or hybrid image selected;" >&2
        echo "      the other images install no packages to export." >&2
    fi
fi

echo
if ((push)); then
    echo "==> pushed $images as ${tags[*]} to $registry"
elif ((containerd_store)) || [[ $platforms != *,* ]]; then
    echo "==> built and loaded $images for $platforms"
else
    echo "==> built $images for $platforms (discarded; --push to publish)"
fi
if [[ -n $packages_dir ]] && ((packaged)); then
    echo "==> packages written to $packages_dir (install with: sudo pacman -U $packages_dir/*.pkg.tar.zst)"
fi
