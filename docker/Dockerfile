# Build stage
FROM rust:latest as builder

# Install dependencies
RUN apt-get update && \
    apt-get install -y cmake build-essential libsqlite3-dev pkg-config libssl-dev

# Clone repository
RUN git clone https://github.com/MostroP2P/mostro.git /mostro

# Set working directory
WORKDIR /mostro

# Build the project in release mode
RUN cargo build --release

# Production stage
FROM rust:slim-bookworm

# Copy builded binary from build stage
COPY --from=builder /mostro/target/release/mostrod /usr/local/bin/mostrod

# Copy settings and empty database
COPY ./settings.docker.toml /mostro/settings.docker.toml
COPY ./empty.mostro.db /mostro/empty.mostro.db

# Copy start script
COPY start.sh /mostro/start.sh
RUN chmod +x /mostro/start.sh

# Start mostro (copy settings and database if it's not created yet)
CMD ["/mostro/start.sh"]
