# syntax=docker/dockerfile:1

# Multi-stage Dockerfile for proposer
# Uses cargo-chef for efficient layer caching

# Stage 1: Chef - prepare recipe
FROM rust:1.88-bookworm AS chef
RUN cargo install cargo-chef
WORKDIR /app

# Stage 2: Planner - create build recipe
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Builder - build the application
FROM chef AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy recipe and build dependencies first (cached layer)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Copy source code
COPY . .

# Build the application
RUN cargo build --release --bin proposer

# Stage 4: Runtime - minimal runtime image
FROM ubuntu:24.04 AS runtime

# Install runtime dependencies and create non-root user
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -s /sbin/nologin proposer

# Copy binary from builder
COPY --from=builder /app/target/release/proposer /usr/local/bin/proposer

# Set user and working directory
USER proposer
WORKDIR /home/proposer

# Expose metrics port
EXPOSE 9090

# Set entrypoint
ENTRYPOINT ["/usr/local/bin/proposer"]
