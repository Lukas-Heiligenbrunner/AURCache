#!/bin/bash
# Produce the source tarball a PKGBUILD expects, from a working tree.
#
# The PKGBUILDs source a release tarball, which is right for a release and
# useless for building the current tree. makepkg only downloads a source it
# does not already have, so writing the tarball into the build directory under
# the expected name makes it build *this* code with no second PKGBUILD and no
# alternate code path -- the builds then pass --skipinteg, since a tree is not
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
# read it". $DEST may well be inside $ROOT.
STAGE=$(mktemp -d)
readonly STAGE
trap 'rm -rf "$STAGE"' EXIT
readonly ARCHIVE="$STAGE/$PKGNAME-$PKGVER.tar.gz"

# A checkout archives what git considers part of it: the tracked files, plus
# untracked ones that are not ignored, so work in progress builds too. That
# needs no list kept up to date, which is why it is preferred: an exclude list
# only leaves out what someone thought of, and a checkout grows build output,
# frontend bundles and whole agent worktrees (one was 11 GB) that nobody did.
# A submodule is listed as its directory alone, which --no-recursion keeps
# empty: makepkg reads the PKGBUILD from its build directory, never from the
# tarball. A tracked file deleted in the working tree is skipped rather than
# failing tar.
#
# The image build has no .git (see .dockerignore, which has already left out
# much of what is excluded below), so it falls back to the whole tree minus
# build output. The `S` flag keeps --transform off symlink targets, which are
# relative to the link rather than to the tree.
if [[ $(git -C "$ROOT" rev-parse --show-toplevel 2>/dev/null) == "$(realpath "$ROOT")" ]]; then
    git -C "$ROOT" ls-files -z --cached --others --exclude-standard |
        while IFS= read -r -d '' path; do
            if [[ -e $ROOT/$path || -L $ROOT/$path ]]; then printf '%s\0' "$path"; fi
        done >"$STAGE/files"
    tar czf "$ARCHIVE" --transform "s,^,$TOP/,S" \
        -C "$ROOT" --no-recursion --null -T "$STAGE/files"
else
    # makepkg's own output is left out too: a build directory inside the tree
    # would otherwise carry the extracted sources, build trees and archives of
    # every package built before it.
    tar czf "$ARCHIVE" --transform "s,^\\.,$TOP,S" \
        --exclude=./.git \
        --exclude=./.claude \
        --exclude=./target \
        --exclude=./backend/target \
        --exclude=./frontend-rs/target \
        --exclude=./frontend-rs/dist \
        --exclude=./backend/aurcache-api/web \
        --exclude=./docs \
        --exclude=./design \
        --exclude='./packaging/*/src' \
        --exclude='./packaging/*/pkg' \
        --exclude='./packaging/*/*.tar.*' \
        -C "$ROOT" .
fi

mv "$ARCHIVE" "$DEST/"
