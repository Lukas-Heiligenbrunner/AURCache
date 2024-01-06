FROM ghcr.io/cirruslabs/flutter:latest AS frontend_builder
WORKDIR /app

COPY frontend /app
RUN flutter build web --release

FROM rust AS builder

# Install necessary tools and dependencies

WORKDIR /app

# Copy the Rust project files
COPY backend/src /app/src
COPY backend/scripts /app/scripts
COPY backend/Cargo.lock /app
COPY backend/Cargo.toml /app

COPY --from=frontend_builder /app/build/web /app/web

# Build the Rust binary
RUN cargo build --release --features static

# Stage 2: Create the final image
FROM archlinux

# Copy the built binary from the previous stage
COPY --from=builder /app/target/release/untitled /usr/local/bin/untitled

RUN echo $'\n\
[multilib]\n\
Include = /etc/pacman.d/mirrorlist\n\
\n\n\
[repo]\n\
SigLevel = Optional TrustAll\n\
Server = http://localhost:8080/' >> /etc/pacman.conf

RUN pacman -Syyu --noconfirm
RUN pacman-key --init && pacman-key --populate
RUN pacman -S --noconfirm base-devel git
RUN pacman -Sc

# Set the entry point or default command to run your application
WORKDIR /app
CMD ["untitled"]
