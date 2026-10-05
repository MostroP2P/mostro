# Toolchain stage, shared by the planner and the builder. Its Debian release
# must match the runtime stage: a newer glibc here makes mostrod fail to start
FROM rust:1.94-bookworm AS chef

# Install build dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends cmake build-essential libsqlite3-dev pkg-config libssl-dev protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /mostro

# The toolchain the project pins is installed here, once, so the dependencies
# and Mostro are compiled by the same rustc: a different one in the final build
# would recompile every dependency
COPY rust-toolchain.toml ./
RUN rustup toolchain install

RUN cargo install cargo-chef --version 0.1.78 --locked

# Planner stage: reduce the manifests to a recipe that only changes when the
# dependencies do
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Build stage
FROM chef AS builder

# Compile the dependencies in their own layer, so a release that only changes
# Mostro's sources reuses them from the cache
COPY --from=planner /mostro/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Copy source code
COPY . .

# Build the project in release mode
RUN cargo build --release

# Production stage
FROM debian:bookworm-slim

# Install dependencies
RUN apt-get update && apt-get install -y --reinstall ca-certificates

# Add a non-root user
RUN useradd -m mostrouser

# Copy built binary from build stage
COPY --from=builder /mostro/target/release/mostrod /usr/local/bin/mostrod

WORKDIR /home/mostrouser

# Default config for start.sh to copy into /config when missing (DB created at startup)
RUN mkdir -p /mostro
COPY ./settings.tpl.toml /mostro/settings.toml
RUN chown -R mostrouser:mostrouser /mostro

# Copy start script
COPY ./docker/start.sh ./start.sh
RUN chmod +x ./start.sh

RUN chown -R mostrouser:mostrouser /home/mostrouser

# Switch to non-root user
USER mostrouser

# Start mostro
CMD ["./start.sh"]
