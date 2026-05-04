#!/usr/bin/env bash
set -e

PACKAGE="${1:-hello}"
BUILDER_IMAGE="${2:-aurcache-builder:test}"
BUILD_FLAGS="${3--B --noconfirm --noprogressbar --color never --pgpfetch}"

docker build -f docker/builder.Dockerfile -t $BUILDER_IMAGE .

TEMP_DIR=$(mktemp -d)
BUILD_DIR="$TEMP_DIR/test_builds"
MAKEPKG_CONF="/var/ab/.config/pacman/makepkg.conf"

cleanup() {
    rm -rf "$BUILD_DIR" 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$BUILD_DIR"
chmod 777 "$BUILD_DIR"

echo "=== Testing builder image: $BUILDER_IMAGE ==="
echo "Building package: $PACKAGE"
echo "Build flags: $BUILD_FLAGS"

docker run --rm \
    -v "$BUILD_DIR:/build" \
    --user ab \
    "$BUILDER_IMAGE" sh -c "
        mkdir -p /build/src
        cd /build/src

        # Write makepkg config (same as aurcache)
        cat > $MAKEPKG_CONF << EOF
MAKEFLAGS=-j\$(nproc)
PKGDEST=/build
EOF

        # Run exact command that aurcache runs:
        # 1. Self-update
        # 2. Download PKGBUILD
        # 3. Build package
        paru -Syu --noconfirm --noprogressbar --color never
        paru -G $PACKAGE
        paru $BUILD_FLAGS *
    "

echo "=== Checking built package ==="
PKGFILE=$(ls -1 "$BUILD_DIR"/*.pkg.tar.* 2>/dev/null | head -1)
if [ -z "$PKGFILE" ]; then
    echo "ERROR: No package file found in $BUILD_DIR"
    ls -la "$BUILD_DIR"
    exit 1
fi

echo "Found package: $PKGFILE"

if tar -tf "$PKGFILE" > /dev/null 2>&1; then
    echo "Archive is valid"
else
    echo "ERROR: Invalid archive file"
    exit 1
fi

TEMP_PARENT=$(dirname "$TEMP_DIR")
docker run --rm -v "$TEMP_PARENT:$TEMP_PARENT" archlinux bash -c " rm -rf '$TEMP_DIR' "
echo "=== Builder test complete ==="
