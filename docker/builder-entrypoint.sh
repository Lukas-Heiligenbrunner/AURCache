#!/bin/bash
# Give this build container a pacman keyring master key of its own, then run
# the build.
#
# The image ships none (see pacman-keyring.sh), and every build starts with a
# `pacman -Syu`. When that upgrades archlinux-keyring, its install script runs
# `pacman-key --populate`, which with no private key stops before doing
# anything at all: new packager keys are not imported and revoked ones are not
# disabled, not just new master keys left untrusted. So one is made here, per
# container -- about a second natively, more under emulation, next to a build.
#
# An entrypoint rather than part of the build command, so that the image is
# safe whichever worker starts it: the command comes from the worker, which is
# released separately.
set -euo pipefail

# The build user may run pacman-key through sudo (see add-aur.sh). Quiet unless
# it fails: otherwise every build log would open with gpg's chatter.
if ! out=$(sudo pacman-key --init 2>&1); then
    printf '%s\n' "$out" >&2
    echo "error: could not generate this build container's pacman keyring master key" >&2
    exit 1
fi

exec "$@"
