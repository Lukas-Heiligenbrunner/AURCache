ARG TARGET_ARCH=linux/amd64

FROM ghcr.io/cirruslabs/flutter:3.22.2 AS frontend_builder
WORKDIR /app

COPY frontend /app
RUN flutter build web --release

FROM rust:1.80.1 AS builder

# Install necessary tools and dependencies

WORKDIR /app

# Copy the Rust project files
COPY backend/src /app/src
COPY backend/Cargo.lock /app
COPY backend/Cargo.toml /app

COPY --from=frontend_builder /app/build/web /app/web

ARG TARGET_ARCH
ADD docker/build-rust.sh /root
RUN bash /root/build-rust.sh $TARGET_ARCH

FROM --platform=$TARGET_ARCH quay.io/podman/stable
# Copy the built binary from the previous stage
COPY --from=builder --chmod=0755 /app/target/aurcache /usr/local/bin/aurcache
COPY --chmod=0755 docker/entrypoint.sh /entrypoint.sh

WORKDIR /app
CMD /entrypoint.sh
