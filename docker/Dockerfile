ARG TARGET_ARCH=linux/amd64
ARG LATEST_COMMIT_SHA=dev

FROM ghcr.io/cirruslabs/flutter:3.32.1 AS frontend_builder
WORKDIR /app

COPY frontend /app
RUN flutter pub get
RUN flutter pub run build_runner build --delete-conflicting-outputs
RUN flutter build web --release --wasm

FROM rust:1.87.0 AS builder
ARG LATEST_COMMIT_SHA
ENV LATEST_COMMIT_SHA ${LATEST_COMMIT_SHA}
# Install necessary tools and dependencies

WORKDIR /app

# Copy the Rust project files
COPY backend/ /app/
COPY --from=frontend_builder /app/build/web /app/aurcache/web

ARG TARGET_ARCH
ADD docker/build-rust.sh /root
RUN bash /root/build-rust.sh $TARGET_ARCH

FROM --platform=$TARGET_ARCH quay.io/podman/stable:v5.5.0
# Copy the built binary from the previous stage
COPY --from=builder --chmod=0755 /app/target/aurcache /usr/local/bin/aurcache
COPY --chmod=0755 docker/entrypoint.sh /entrypoint.sh

WORKDIR /app
CMD /entrypoint.sh
