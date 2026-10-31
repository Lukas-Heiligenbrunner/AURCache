#!/bin/bash
# Build and install the armv7h cross toolchain from the AUR.
#
# Arch ships aarch64's cross toolchain in `extra` but not armv7h's, so this
# builds the AUR chain instead. gcc is compiled three times, because that is
# what bootstrapping a cross compiler takes: a stage-1 gcc builds the glibc
# headers, a stage-2 gcc builds glibc proper, and only then can the real gcc be
# built against it.
#
# Usage: build-cross-toolchain.sh armv7h
set -euo pipefail

readonly CARCH_TARGET=${1:?usage: build-cross-toolchain.sh <carch>}

if [[ $CARCH_TARGET != armv7h ]]; then
    printf 'build-cross-toolchain.sh: only armv7h is built from source; %s comes from the official repositories\n' \
        "$CARCH_TARGET" >&2
    exit 1
fi

# Dependency order, from each package's own metadata rather than guesswork.
readonly PACKAGES=(
    arm-linux-gnueabihf-linux-api-headers
    arm-linux-gnueabihf-binutils
    arm-linux-gnueabihf-gcc-stage1
    arm-linux-gnueabihf-glibc-headers
    arm-linux-gnueabihf-gcc-stage2
    arm-linux-gnueabihf-glibc
    arm-linux-gnueabihf-gcc
)

# An optional pacman repository holding prebuilt cross-toolchain packages --
# an AURCache instance, for example, which is a pleasant way to avoid the
# bootstrap entirely. Each package is taken from there when present and built
# from the AUR only when it is not, so a repository that carries some of the
# chain still saves most of the work.
readonly REPO_URL=${AURCACHE_TOOLCHAIN_REPO:-}
# pacman derives the database filename from the section name, so this has to be
# what the repository actually serves: an AURCache instance publishes `repo.db`
# because it initialises its repository as `repo`, which is why an AURCache
# client's pacman.conf says `[repo]`. Override for a repository named otherwise.
readonly REPO_NAME=${AURCACHE_TOOLCHAIN_REPO_NAME:-repo}

if [[ -n $REPO_URL ]]; then
    printf '==> using prebuilt packages from %s where available\n' "$REPO_URL"
    # TrustAll because these repositories are typically unsigned. That is the
    # same trust already placed in the AUR fallback below, which fetches
    # PKGBUILDs over HTTPS with no signature at all -- but it does mean the
    # repository must be one you control.
    printf '\n[%s]\nSigLevel = Optional TrustAll\nServer = %s\n' "$REPO_NAME" "$REPO_URL" \
        | sudo tee -a /etc/pacman.conf >/dev/null
    sudo pacman -Sy --noconfirm
fi

# Remove any installed package that `$1..` supersede.
#
# Each gcc stage `replaces` the one before it, and `pacman --noconfirm` answers
# a conflict prompt with its *default*, which for "Remove <conflicting
# package>?" is No. Without this, installing stage2 over stage1 simply fails --
# on the prebuilt path in a fraction of a second, and on the source path only
# after building gcc for the better part of an hour.
_remove_superseded() {
    local old_pkg
    for old_pkg in "$@"; do
        [[ -z $old_pkg ]] && continue
        if pacman -Qq "$old_pkg" >/dev/null 2>&1; then
            printf '    removing superseded %s\n' "$old_pkg"
            # -dd: what supersedes it is not installed yet, so a dependency
            # check would refuse a removal that is about to be made good.
            sudo pacman -Rdd --noconfirm "$old_pkg"
        fi
    done
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# One source cache for the whole chain, which is what stops gcc's git repository
# being cloned three times and glibc's twice.
#
# makepkg names a git source's local mirror after the URL's basename, so
# gcc-stage1, gcc-stage2 and gcc all want a clone called `gcc`, and the two
# glibc packages both want `glibc`. Given a shared SRCDEST, the first package
# clones and the rest fetch into the existing mirror. The final gcc names the
# GitHub mirror rather than sourceware, but makepkg fetches from the remote the
# clone already has -- the same repository, and the commit it wants is there.
#
# Set through ~/.makepkg.conf rather than the environment: makepkg sources its
# configuration after the environment, so a config file is the mechanism that
# is actually documented to win.
export SRCDEST="$work/srcdest"
mkdir -p "$SRCDEST"
printf 'SRCDEST=%s\n' "$SRCDEST" >> "$HOME/.makepkg.conf"

for pkg in "${PACKAGES[@]}"; do
    # Prefer the prebuilt package, qualified by repository so a same-named
    # package in core/extra can never be picked up by accident.
    if [[ -n $REPO_URL ]] && pacman -Si "$REPO_NAME/$pkg" >/dev/null 2>&1; then
        # `Replaces` comes from the repository's own metadata, so the prebuilt
        # path needs no PKGBUILD to know what to clear out of the way.
        mapfile -t superseded < <(
            pacman -Si "$REPO_NAME/$pkg" 2>/dev/null |
                awk -F': *' '/^Replaces/ {print $2}' |
                tr ' ' '\n' | grep -v '^None$' | grep -v '^$'
        )
        _remove_superseded "${superseded[@]:-}"

        if sudo pacman -S --noconfirm --needed "$REPO_NAME/$pkg"; then
            printf '==> %s: installed prebuilt\n' "$pkg"
            continue
        fi
        printf '    prebuilt install failed, building instead\n'
    fi

    printf '==> building %s\n' "$pkg"
    git clone --depth 1 "https://aur.archlinux.org/$pkg.git" "$work/$pkg"
    cd "$work/$pkg"

    # Every gcc package here builds gcc's bundled `libcody`, which indexes
    # string literals as `char`; a compiler defaulting to C++20 makes `u8"..."`
    # a `char8_t` array and the call no longer matches:
    #
    #   libcody/buffer.cc:381: no matching function for call to
    #     'S2C(const char8_t [2])'  -- mismatched 'const char' and 'const char8_t'
    #
    # so all three gcc packages need the flag, not just stage1. Applied as an
    # edit rather than a patch file because it is one flag and a diff would rot
    # against an AUR package that gets version-bumped.
    if [[ $pkg == *-gcc-stage1 || $pkg == *-gcc-stage2 || $pkg == arm-linux-gnueabihf-gcc ]]; then
        printf '    adding -fno-char8_t to CXXFLAGS\n'
        sed -i '/^build()/a\  CXXFLAGS+=" -fno-char8_t"' PKGBUILD
        grep -q 'fno-char8_t' PKGBUILD || {
            echo "failed to patch $pkg: build() not found where expected" >&2
            exit 1
        }
    fi

    # These packages check upstream PGP signatures (kernel.org, GNU) and a
    # fresh container has an empty keyring. Import exactly the keys the PKGBUILD
    # declares rather than passing --skippgpcheck: the signature is the only
    # check that says the tarball came from its author, since the sha256sums
    # next to it are supplied by the same AUR repository the PKGBUILD is.
    #
    # Sourcing a PKGBUILD executes it, which is acceptable here only because
    # makepkg is about to do the same thing to the same file.
    # shellcheck source=/dev/null
    mapfile -t keys < <(set +u; . ./PKGBUILD; printf '%s\n' "${validpgpkeys[@]:-}")
    if (( ${#keys[@]} )) && [[ -n ${keys[0]} ]]; then
        printf '    importing %d signing key(s)\n' "${#keys[@]}"
        gpg --batch --quiet --keyserver keyserver.ubuntu.com --recv-keys "${keys[@]}"
    fi

    # The three gcc packages share one source cache, and makepkg keys a git
    # source's local mirror on the URL's basename -- so all three want a clone
    # called `gcc`. Two name sourceware and the final one names the GitHub
    # mirror, and makepkg checks an existing clone's origin and refuses when it
    # differs ("is not a clone of ..."). Pointing them all at the canonical
    # remote is what makes one clone serve all three; it is the same repository
    # and the same commit either way.
    sed -i 's|git+https://github.com/gcc-mirror/gcc.git|git+https://sourceware.org/git/gcc.git|' PKGBUILD

    # gmplib.org refuses connections often enough to fail this build outright.
    # gmp is a GNU project and ftp.gnu.org carries the identical tarball and
    # signature -- same sha256 as the PKGBUILD records, checked before this was
    # written. The mirror is only a delivery route: makepkg still verifies the
    # checksum and the PGP signature, so a substituted file fails the build
    # rather than getting into it.
    if [[ $pkg == arm-linux-gnueabihf-gcc ]]; then
        printf '    sourcing gmp from ftp.gnu.org rather than gmplib.org\n'
        sed -i 's|https://gmplib.org/download/gmp/|https://ftp.gnu.org/gnu/gmp/|' PKGBUILD
    fi

    # Built and installed as two steps rather than with `--install`.
    #
    # Each gcc stage `replaces` the one before it, and `pacman --noconfirm`
    # answers a conflict prompt with its *default* -- which for "Remove
    # <conflicting package>?" is No. So `makepkg --install` builds for the
    # better part of an hour and then refuses to install what it just built.
    # Removing the superseded package first turns that into a no-op.
    #
    # Retried because the sources come from half a dozen hosts, several of which
    # rate-limit: sourceware, GitHub, sourceforge, gnu.org, mpfr.org. A failure
    # here is far more often a refused connection than anything about the build,
    # and the already-downloaded sources make a second attempt cheap.
    attempt=1
    until makepkg --syncdeps --noconfirm --needed --noprogressbar; do
        if (( attempt >= 3 )); then
            printf 'aurcache: %s failed after %d attempts\n' "$pkg" "$attempt" >&2
            exit 1
        fi
        printf '    attempt %d failed, retrying in 30s\n' "$attempt"
        sleep 30
        attempt=$(( attempt + 1 ))
    done

    # Drop whatever this package supersedes, then install it.
    # shellcheck source=/dev/null
    mapfile -t superseded < <(set +u; . ./PKGBUILD; printf '%s\n' "${replaces[@]:-}")
    _remove_superseded "${superseded[@]:-}"
    sudo pacman -U --noconfirm --needed ./*.pkg.tar.zst
    cd /
done

printf '==> cross toolchain for %s installed\n' "$CARCH_TARGET"
