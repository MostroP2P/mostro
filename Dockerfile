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

# Set working directory
WORKDIR /home/.mostro

# Copy built binary from build stage
COPY --from=builder /mostro/target/release/mostrod /usr/local/bin/mostrod

# Copy empty database
COPY ./mostro.db ./

# Start mostro
CMD ["mostrod"]
