# Build stage
FROM rust:1.82-alpine AS builder

# Install build dependencies
RUN apk update && \
    apk add --no-cache cmake build-base sqlite-dev pkgconfig openssl-dev protoc perl git

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
FROM alpine:latest

# Install runtime dependencies
RUN apk add --no-cache ca-certificates sqlite-libs openssl

# Add a non-root user and create home directory
RUN adduser -D -h /home/mostrouser mostrouser

WORKDIR /home/mostrouser

# Copy built binary from build stage
COPY --from=builder /mostro/target/release/mostrod /usr/local/bin/mostrod

# Copy settings and empty database
COPY ./docker/settings.docker.toml ./docker/empty.mostro.db ./

# Copy start script
COPY ./scripts/entrypoint.sh ./entrypoint.sh
RUN chmod +x ./entrypoint.sh

RUN chown -R mostrouser:mostrouser /home/mostrouser

# Switch to non-root user
USER mostrouser

# Start mostro (copy settings and database if it's not created yet)
CMD ["./entrypoint.sh"]
