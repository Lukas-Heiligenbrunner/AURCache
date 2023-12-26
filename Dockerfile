# Stage 1: Build the Rust binary
FROM rust AS builder

# Install necessary tools and dependencies

WORKDIR /app

# Copy the Rust project files
COPY ./src /app/src
COPY ./scripts /app/scripts
COPY Cargo.lock /app
COPY Cargo.toml /app

# Build the Rust binary
RUN cargo build --release

# Stage 2: Create the final image
FROM archlinux

# Copy the built binary from the previous stage
COPY --from=builder /app/target/release/untitled /usr/local/bin/untitled

RUN pacman -Syyu --noconfirm
RUN pacman -S --noconfirm base-devel git
RUN pacman -Sc

# Set the entry point or default command to run your application
WORKDIR /app
CMD ["untitled"]
