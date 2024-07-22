FROM ghcr.io/cirruslabs/flutter:3.22.2 AS frontend_builder
WORKDIR /app

COPY frontend /app
RUN flutter build web --release

FROM rust AS builder

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
COPY --from=builder /app/target/release/aurcache /usr/local/bin/aurcache

RUN dnf -y install pacman  && dnf clean all
COPY entrypoint.sh /entrypoint.sh

RUN chmod +x /entrypoint.sh /usr/local/bin/aurcache

# Set the entry point or default command to run your application
WORKDIR /app
CMD /entrypoint.sh
