#!/bin/bash
# Install AURCache packages the packager stage built, plus any packages from the
# repositories, in one transaction against a freshly synced database.
#
# Usage: aurcache-install-packages DIR NAME... [-- REPO-PACKAGE...]
#
# DIR holds the archives (docker/arch.Dockerfile bind-mounts the packager's
# /pkg there, so they never land in a layer); each NAME is a package whose
# archive is in it.
#
# The sync database is refreshed *here* rather than inherited from the runtime
# stage, even though that stage has just done the same thing. `pacman -U`
# resolves the packages' dependencies against whatever database the image
# carries, and the layer that populated it is cached across builds while the
# repositories behind it are not: Arch Linux ARM rotates a superseded package
# out of its pool within weeks. So a code change invalidates the install layer,
# the cached database still names python-dulwich-1.1.0-1, and every mirror
# answers 404 for a file none of them has kept -- which reads as the mirrors
# being broken rather than as the database being stale. Re-syncing in the same
# layer as the install is what keeps the two from drifting apart.
#
# That `-Syu` can upgrade archlinux-keyring, whose install script needs a master
# key to trust a new distribution key: one is made for this layer and deleted
# with it (see pacman-keyring.sh).
#
# systemd-sysusers / systemd-tmpfiles run explicitly as well as through
# pacman's hooks: an image with no systemd manager is exactly where a hook that
# quietly did not run would go unnoticed until a build failed for want of a
# directory. Both are idempotent.
set -euo pipefail

dir=${1:?usage: aurcache-install-packages DIR NAME... [-- REPO-PACKAGE...]}
shift

archives=()
while (($#)) && [[ $1 != -- ]]; do
    # `-[0-9]` because the version follows the name: `aurcache-worker-*` alone
    # would also match aurcache-worker-docker.
    matches=("$dir/$1"-[0-9]*.pkg.tar.zst)
    if [[ ! -e ${matches[0]} ]]; then
        echo "aurcache-install-packages: no $1 archive in $dir" >&2
        exit 1
    fi
    archives+=("${matches[@]}")
    shift
done
(($#)) && shift
repo_packages=("$@")

aurcache-pacman-keyring ensure
pacman -Syu --noconfirm --needed
pacman -U --noconfirm "${archives[@]}"
if ((${#repo_packages[@]})); then
    pacman -S --noconfirm --needed "${repo_packages[@]}"
fi
aurcache-pacman-keyring forget
systemd-sysusers
systemd-tmpfiles --create
