# shellcheck shell=bash
# Shared PKGBUILD helpers: how to build this repo's Rust binaries for $CARCH.
#
# Sourced by both PKGBUILDs rather than duplicated, so the two cannot disagree
# about targets, flags or where the output lands.

# Arch has no cross-compilation feature -- no `--target`, nothing in devtools --
# but `CARCH` is a plain shell variable makepkg sources and uses for the package
# name, so setting it and cross-compiling inside build() produces a correctly
# labelled package. That is a convention, not a supported mode, which is why it
# is written down here rather than assumed.
# Whether $CARCH is the machine we are running on.
#
# Not a plain string compare: Arch calls 32-bit ARM `armv7h` while `uname -m`
# says `armv7l`, so a native armv7 build would otherwise look like a cross one
# and demand a cross toolchain that a native host has no reason to install.
_aurcache_is_native() {
    local host
    host=$(uname -m)
    [[ $CARCH == "$host" ]] && return 0
    [[ $CARCH == armv7h && $host == armv7l ]] && return 0
    return 1
}

# The Rust target triple for the architecture makepkg is building for.
_aurcache_rust_target() {
    case "$CARCH" in
        x86_64) printf 'x86_64-unknown-linux-gnu' ;;
        aarch64) printf 'aarch64-unknown-linux-gnu' ;;
        armv7h) printf 'armv7-unknown-linux-gnueabihf' ;;
        *)
            printf 'aurcache: no Rust target known for CARCH=%s\n' "$CARCH" >&2
            return 1
            ;;
    esac
}

# The GNU triple of the cross toolchain for $CARCH, which is not the Rust one:
# Rust says `armv7-unknown-linux-gnueabihf` where the toolchain says
# `arm-linux-gnueabihf`.
_aurcache_cross_prefix() {
    case "$CARCH" in
        aarch64) printf 'aarch64-linux-gnu' ;;
        armv7h) printf 'arm-linux-gnueabihf' ;;
        *)
            printf 'aurcache: no cross toolchain known for CARCH=%s\n' "$CARCH" >&2
            return 1
            ;;
    esac
}

# Point cargo and the `cc` crate at a cross toolchain for a foreign $CARCH.
#
# aarch64's toolchain is in Arch's `extra`; armv7h's is in the AUR
# (arm-linux-gnueabihf-{binutils,gcc,glibc,linux-api-headers}). Either way this
# refuses with a readable message rather than failing later in a linker error
# nobody can place.
_aurcache_setup_cross() {
    local target=$1
    _aurcache_is_native && return 0

    local prefix
    prefix=$(_aurcache_cross_prefix) || return 1

    if ! command -v "$prefix-gcc" >/dev/null; then
        printf 'aurcache: cross-compiling to %s needs %s-gcc\n' "$CARCH" "$prefix" >&2
        return 1
    fi

    # cargo wants the triple upper-cased with dashes as underscores; the `cc`
    # crate wants it lower-cased the same way.
    local upper=${target^^}
    upper=${upper//-/_}
    local lower=${target//-/_}

    export "CARGO_TARGET_${upper}_LINKER=$prefix-gcc"
    export "CC_${lower}=$prefix-gcc"
    export "AR_${lower}=$prefix-ar"

    # makepkg's CFLAGS are tuned for the *build host* -- `-march=x86-64` and
    # friends -- and the `cc` crate passes them straight through to the cross
    # compiler, which rejects them outright ("unknown value 'x86-64' for
    # '-march'"). cc prefers a target-specific variable over the generic one,
    # so give the target its own flags rather than inheriting the host's.
    export "CFLAGS_${lower}=-O2 -pipe -fno-plt -fexceptions"
    export "CXXFLAGS_${lower}=-O2 -pipe -fno-plt -fexceptions"

    # ...and clear the generic ones outright, because overriding is not enough.
    # `aws-lc-sys` builds its compiler-probe commands straight from $CFLAGS and
    # $LDFLAGS instead of going through the cc crate, so it picks up the host's
    # `-march` however carefully the target-specific variables are set. Nothing
    # in a cross build wants flags chosen for the build machine; cc falls back
    # to its own defaults for the host build scripts.
    unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS
}

# Where cargo leaves the binaries for $CARCH, relative to backend/.
#
# Its own function because makepkg runs package() in a separate fakeroot
# process that re-sources the PKGBUILD: anything build() assigned is gone by
# then, so the path has to be derivable rather than remembered.
_aurcache_release_dir() {
    local target
    target=$(_aurcache_rust_target) || return 1
    printf 'target/%s/release' "$target"
}

# Build the named workspace packages for $CARCH.
_aurcache_cargo_build() {
    local target
    target=$(_aurcache_rust_target) || return 1
    _aurcache_setup_cross "$target" || return 1

    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    # Strip through cargo rather than makepkg: makepkg would run the *host*
    # binutils over a foreign binary.
    export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C strip=symbols"

    cargo build --frozen --release --target "$target" "$@"
}
