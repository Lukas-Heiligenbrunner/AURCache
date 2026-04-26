ARG TARGETARCH
ARG TARGETVARIANT
ARG TARGETPLATFORM

########## Base images ##########
FROM --platform=linux/amd64 archlinux/archlinux:latest AS arch_amd64
FROM --platform=linux/arm64 lopsided/archlinux:latest AS arch_arm64
FROM --platform=linux/riscv64 ogarcia/archlinux:latest AS arch_riscv64
FROM --platform=linux/arm/v7 lopsided/archlinux-arm32v7:latest AS arch_armv7

########## Target sysroot (extract libs for cross-compilation) ##########
FROM arch_${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS target_sysroot
RUN pacman -Syu --noconfirm gcc

########## Cross-compile paru on amd64 ##########
FROM --platform=linux/amd64 archlinux/archlinux:latest AS paru_builder
ARG TARGETARCH
ARG TARGETVARIANT

RUN pacman -Syu --noconfirm base-devel clang lld rustup

COPY --from=target_sysroot /usr/lib/ /target-sysroot/usr/lib/
COPY --from=target_sysroot /usr/include/ /target-sysroot/usr/include/

RUN rustup default stable
RUN rustup target add aarch64-unknown-linux-gnu armv7-unknown-linux-gnueabihf riscv64gc-unknown-linux-gnu

RUN <<'SCRIPT'
set -eux
ARCH="${TARGETARCH}${TARGETVARIANT}"

# Map Docker arch to Rust target and clang target triple
case "$ARCH" in
  amd64)   RUST_TARGET=x86_64-unknown-linux-gnu;      CLANG_TARGET="" ;;
  arm64)   RUST_TARGET=aarch64-unknown-linux-gnu;      CLANG_TARGET=aarch64-linux-gnu ;;
  armv7)   RUST_TARGET=armv7-unknown-linux-gnueabihf;  CLANG_TARGET=armv7-linux-gnueabihf ;;
  riscv64) RUST_TARGET=riscv64gc-unknown-linux-gnu;    CLANG_TARGET=riscv64-linux-gnu ;;
  *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# Set up cross-compilation for non-amd64 targets
if [ -n "$CLANG_TARGET" ]; then
  # Find GCC CRT/runtime directory in target sysroot
  GCC_DIR=$(find /target-sysroot/usr/lib/gcc -name crtbeginS.o -printf '%h\n' 2>/dev/null | head -1 || true)
  GCC_FLAGS="${GCC_DIR:+-B$GCC_DIR -L$GCC_DIR}"

  # Create clang cross-compiler wrapper
  printf '#!/bin/sh\nexec clang --target=%s --sysroot=/target-sysroot -fuse-ld=lld %s "$@"\n' "$CLANG_TARGET" "$GCC_FLAGS" \
    > /usr/local/bin/clang-cross && chmod +x /usr/local/bin/clang-cross

  # Configure cargo, cc, bindgen, and pkg-config for the target
  TARGET_UPPER=$(echo "$RUST_TARGET" | tr 'a-z-' 'A-Z_')
  TARGET_UNDER=$(echo "$RUST_TARGET" | tr '-' '_')
  export "CARGO_TARGET_${TARGET_UPPER}_LINKER=/usr/local/bin/clang-cross"
  export "CC_${TARGET_UNDER}=/usr/local/bin/clang-cross"
  export "BINDGEN_EXTRA_CLANG_ARGS_${TARGET_UNDER}=--target=$CLANG_TARGET --sysroot=/target-sysroot"
  export "PKG_CONFIG_SYSROOT_DIR_${TARGET_UNDER}=/target-sysroot"
  export "PKG_CONFIG_LIBDIR_${TARGET_UNDER}=/target-sysroot/usr/lib/pkgconfig"
  export PKG_CONFIG_ALLOW_CROSS=1
fi

cargo install --features "generate" --git https://github.com/gyscos/paru --target="$RUST_TARGET"
SCRIPT

########## Select correct base ##########
FROM arch_${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS final

ARG TARGETARCH
ARG TARGETVARIANT
ARG TARGETPLATFORM
ENV TARGETARCH=${TARGETARCH}
ENV TARGETVARIANT=${TARGETVARIANT}
ENV TARGETPLATFORM=${TARGETPLATFORM}

########## Files ##########
ADD docker/add-aur.sh /root
ADD docker/pacman.conf.amd64 /etc/pacman.conf.amd64
COPY --from=paru_builder --chmod=0755 /root/.cargo/bin/paru /usr/local/bin/paru
RUN /bin/bash /root/add-aur.sh ab paru
USER ab