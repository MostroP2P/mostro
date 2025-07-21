# Build stage
FROM rust:1.86-alpine AS builder

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

# Create the mostro data directory
RUN mkdir -p /mostro

# Set working directory to /mostro for persistent data
WORKDIR /mostro

# Copy built binary from build stage
COPY --from=builder /mostro/target/release/mostrod /usr/local/bin/mostrod

# Copy empty database to the mostro directory
COPY ./mostro.db ./

