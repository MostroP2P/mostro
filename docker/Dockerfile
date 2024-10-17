# Build stage
FROM rust:1.81 AS builder

# Set working directory
WORKDIR /mostro

# Copy source code
COPY . .

# Install build dependencies
RUN apt-get update && \
    apt-get install -y cmake build-essential libsqlite3-dev pkg-config libssl-dev protobuf-compiler

# Build the project in release mode
RUN cargo build --release

# Production stage
FROM debian:bookworm-slim

# Copy built binary from build stage
COPY --from=builder /mostro/target/release/mostrod /usr/local/bin/mostrod

WORKDIR /mostro

# Copy settings and empty database
COPY --chown=mostrouser:mostrouser ./docker/settings.docker.toml ./docker/empty.mostro.db ./

# Copy start script
COPY --chown=mostrouser:mostrouser ./docker/start.sh ./start.sh
RUN chmod +x ./start.sh

# Add a non-root user and switch to it
RUN useradd -m mostrouser
USER mostrouser

# Start mostro (copy settings and database if it's not created yet)
ENTRYPOINT ["./start.sh"]
