#!/usr/bin/env bash
# Make sure the rustup toolchain the packager stages cross-compile with is
# installed, along with the targets given as arguments, and serialize every
# change to it.
#
# The rustup home is a buildkit cache mount, which buildkit may evict
# independently of the layer cache: a cached RUN that installed the toolchain
# can outlive the toolchain it installed. So nothing assumes an earlier step
# left one behind -- every RUN that needs the toolchain calls this first, and
# on a warm cache it changes nothing.
#
# The packager always runs on amd64 (see worker.Dockerfile), so every target
# architecture shares the one x86_64 toolchain in that mount, and their builds
# run at the same time. rustup is not safe to run concurrently on one home: a
# component installs with a `rename(2)` over a shared `<hash>.partial` file
# ("could not rename 'downloaded' file", "detected conflict: bin/cargo"), and
# every `target add` rewrites the toolchain's installed-components list, so two
# architectures adding their std at once can each lose the other's. Hence the
# lock, which lives in the mount itself so every build sharing it takes the
# same one. Only rustup is serialized; the compiles that follow run in parallel.
set -eu
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
mkdir -p "$rustup_home"
exec 9>"$rustup_home/.aurcache-install.lock"
flock 9

rustup default stable
for target in "$@"; do
    rustup target add "$target"
done
