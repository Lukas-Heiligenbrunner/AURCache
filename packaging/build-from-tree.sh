#!/usr/bin/env bash
# Build one of the AUR packages from a working tree rather than from its
# release, and print the path of each archive it made.
#
# Usage: build-from-tree.sh <repo-root> <pkgname> <dest-dir>
#
# The build runs in <dest-dir>/<pkgname>, a fresh copy of the PKGBUILD's
# directory, so makepkg leaves its src/, pkg/ and tarball neither in the
# submodule nor in the tree being archived; the archives are moved up into
# <dest-dir>. scripts/build-packages.sh runs this on a host, and the images'
# packager stage through docker/build-aurcache-packages.sh.
set -euo pipefail

# Absolute, since the build changes directory.
ROOT=$(realpath -- "$1")
readonly ROOT PKGNAME=$2
DEST=$(realpath -m -- "$3")
readonly DEST

readonly SRC="$ROOT/packaging/$PKGNAME"
[[ -f $SRC/PKGBUILD ]] || { echo "build-from-tree.sh: no PKGBUILD in $SRC" >&2; exit 2; }

work="$DEST/$PKGNAME"
rm -rf "$work"
mkdir -p "$DEST"
cp -r "$SRC" "$work"
cd "$work"

# The tarball has to carry the name the PKGBUILD's `source=` asks for, so its
# version is read from the PKGBUILD rather than written here. When the two
# disagree, makepkg does not complain: it downloads the release of that version
# from GitHub and builds that instead of this tree.
pkgver=$(awk -F= '/^pkgver=/ {print $2; exit}' PKGBUILD)
"$ROOT/packaging/make-source-tarball.sh" "$ROOT" "$PKGNAME" "$pkgver" .

# `--skipinteg` because the tarball is this tree rather than a release, so it
# cannot match a release checksum. `--nodeps` because the *build* needs nothing
# from the target architecture: dependencies are recorded in the package and
# resolved where it is installed, which is not necessarily here.
makepkg --skipinteg --noconfirm --nocheck --nodeps >&2

# The copy is fresh, so every archive in it is this build's.
for archive in ./*.pkg.tar.zst; do
    mv "$archive" "$DEST/"
    printf '%s\n' "$DEST/${archive#./}"
done
