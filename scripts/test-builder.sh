#!/usr/bin/env bash
set -e

PACKAGE="${1:-hello}"
BUILDER_IMAGE="${2:-aurcache-builder:test}"

docker build -f docker/builder.Dockerfile -t $BUILDER_IMAGE .

TEMP_DIR=$(mktemp -d)
BUILD_DIR="$TEMP_DIR/test_builds"

cleanup() {
    rm -rf "$BUILD_DIR" 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$BUILD_DIR"
chmod 777 "$BUILD_DIR"

echo "=== Testing builder image: $BUILDER_IMAGE ==="
echo "Building package: $PACKAGE"

docker run --rm \
    -v "$BUILD_DIR:/build" \
    --user ab \
    "$BUILDER_IMAGE" sh -c "
        cd /build
        paru -G $PACKAGE
        paru -B --noconfirm --noprogressbar --color never *
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

rm $TEMP_DIR -rf
echo "=== Builder test complete ==="
