#!/bin/bash
# Produce the source tarball a PKGBUILD expects, from a working tree.
#
# The PKGBUILDs source a release tarball, which is right for a release and
# useless for building the current tree. makepkg only downloads a source it
# does not already have, so writing the tarball into the build directory under
# the expected name makes it build *this* code with no second PKGBUILD and no
# alternate code path -- the images then pass --skipinteg, since a tree is not
# going to match a release checksum.
#
# Usage: make-source-tarball.sh <repo-root> <pkgname> <pkgver> <dest-dir>
set -euo pipefail

readonly ROOT=$1
readonly PKGNAME=$2
readonly PKGVER=$3
readonly DEST=$4

# The name the PKGBUILD extracts into.
readonly TOP="AURCache-$PKGVER"

mkdir -p "$DEST"

# Built outside the tree and moved in: writing the archive into a directory it
# is archiving makes tar read its own output and abort with "file changed as we
# read it". $DEST is normally the PKGBUILD's directory, which is inside $ROOT.
STAGE=$(mktemp -d)
readonly STAGE
trap 'rm -rf "$STAGE"' EXIT

tar czf "$STAGE/$PKGNAME-$PKGVER.tar.gz" \
    --transform "s,^\\.,$TOP," \
    --exclude=./.git \
    --exclude=./target \
    --exclude=./backend/target \
    --exclude=./frontend-rs/target \
    --exclude=./frontend \
    --exclude=./docs/node_modules \
    -C "$ROOT" .

mv "$STAGE/$PKGNAME-$PKGVER.tar.gz" "$DEST/"
