ARG LATEST_COMMIT_SHA=dev

FROM ghcr.io/cirruslabs/flutter:3.24.3 AS frontend_builder
WORKDIR /app

COPY frontend /app
RUN flutter build web --release

FROM rust AS builder
ARG LATEST_COMMIT_SHA
ENV LATEST_COMMIT_SHA ${LATEST_COMMIT_SHA}
# Install necessary tools and dependencies

WORKDIR /app

# Copy the Rust project files
COPY backend/src /app/src
COPY backend/Cargo.lock /app
COPY backend/Cargo.toml /app

COPY --from=frontend_builder /app/build/web /app/web

# Build the Rust binary
RUN cargo build --release --features static

# Stage 2: Create the final image
FROM quay.io/podman/stable

# Copy the built binary from the previous stage
COPY --from=builder --chmod=0755 /app/target/release/aurcache /usr/local/bin/aurcache
COPY --chmod=0755 entrypoint.sh /entrypoint.sh

WORKDIR /app
CMD /entrypoint.sh
