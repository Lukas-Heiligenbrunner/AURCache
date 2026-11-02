#!/bin/bash
# Keep the pacman keyring's master key out of the image, and give each
# container one of its own instead.
#
# `pacman-key --init` generates a "Pacman Keyring Master Key": a private key
# the keyring trusts ultimately, which `pacman-key --populate` then uses to
# vouch for the distribution's own master keys. Generated while an image is
# built, it lands in a layer, and everyone who pulls the image holds the same
# private key. With it they can certify a key of their own that pacman then
# trusts -- in every container started from that image, and in every chroot
# `mkarchroot` creates there, since pacstrap copies the keyring in whole. The
# official Arch Linux image ships without that key for exactly this reason.
#
#   forget  Delete the private key material, and fail if any is left. Run it
#           at the end of the same RUN that generated the key: a later layer
#           only hides a file, and the layer before still carries it. What the
#           key signed stays valid -- the distribution's keys keep the trust it
#           gave them -- and nobody holds the key any more to sign anything else.
#
#   ensure  Generate a master key if there is none. pacman needs one to vouch
#           for a *new* distribution master key, which happens when an
#           archlinux-keyring upgrade runs `--populate`; without it, that
#           step fails and the new key is never trusted. Cheap when a key is
#           already there: nothing is regenerated.
set -euo pipefail

readonly GNUPG=/etc/pacman.d/gnupg

has_private_key() {
    [ -n "$(find "$GNUPG/private-keys-v1.d" -mindepth 1 -print -quit 2>/dev/null)" ] ||
        [ -s "$GNUPG/secring.gpg" ]
}

case ${1:-} in
forget)
    # The agent pacman-key started may still hold the key, and left its
    # sockets in the keyring directory.
    gpgconf --homedir "$GNUPG" --kill all 2>/dev/null || true
    rm -f "$GNUPG"/S.*
    # The revocation certificate is the key's, and goes with it.
    find "$GNUPG/private-keys-v1.d" "$GNUPG/openpgp-revocs.d" -mindepth 1 -delete 2>/dev/null || true
    if [ -e "$GNUPG/secring.gpg" ]; then : >"$GNUPG/secring.gpg"; fi
    if has_private_key; then
        echo "error: the pacman keyring still holds a private key in $GNUPG" >&2
        exit 1
    fi
    ;;
ensure)
    has_private_key || pacman-key --init
    ;;
*)
    echo "usage: $0 forget|ensure" >&2
    exit 2
    ;;
esac
