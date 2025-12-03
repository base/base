# --- Chef base image ---
FROM lukemathwalker/cargo-chef:latest-rust-1.88-trixie AS chef
WORKDIR /app

RUN apt-get update && \
    apt-get -y upgrade && \
    apt-get install -y git libclang-dev pkg-config curl build-essential && \
    rm -rf /var/lib/apt/lists/*

# --- Planner: analyze dependencies ---
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- Builder: cook dependencies, then build ---
FROM chef AS builder

# Copy ONLY the recipe (dependency metadata)
COPY --from=planner /app/recipe.json recipe.json

# Build dependencies - THIS LAYER IS CACHED until Cargo.toml/Cargo.lock change
RUN cargo chef cook --release --recipe-path recipe.json

# Now copy source and build (only your code compiles here)
COPY . .
RUN cargo build --release --bin base-reth-node

# --- Runtime image ---
FROM ubuntu:22.04

RUN apt-get update && \
    apt-get install -y jq curl && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/base-reth-node ./

ENTRYPOINT ["./base-reth-node"]
