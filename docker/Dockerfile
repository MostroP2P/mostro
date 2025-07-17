# Build stage
FROM rust:1.86 AS builder

# Install build dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends cmake build-essential libsqlite3-dev pkg-config libssl-dev protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*


# Set working directory
WORKDIR /mostro

# Copy Cargo.toml and Cargo.lock to leverage Docker cache
COPY Cargo.toml Cargo.lock ./
RUN cargo fetch

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

# Copy settings and empty database
COPY ./docker/settings.docker.toml ./docker/empty.mostro.db ./

# Copy start script
COPY ./docker/start.sh ./start.sh
RUN chmod +x ./start.sh

RUN chown -R mostrouser:mostrouser /home/mostrouser

# Switch to non-root user
USER mostrouser

# Start mostro (copy settings and database if it's not created yet)
CMD ["./start.sh"]
