# --- Chef base image ---
FROM lukemathwalker/cargo-chef:latest-rust-1.93-trixie AS chef
WORKDIR /app
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      git libclang-dev pkg-config curl build-essential && \
    rm -rf /var/lib/apt/lists/*

# --- Planner: analyze dependencies ---
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- Builder: cook dependencies, then build ---
FROM chef AS builder
ARG PROFILE=release

# Copy ONLY the recipe (dependency metadata)
COPY --from=planner /app/recipe.json recipe.json
# Create empty frontend directory for cook phase (rust_embed needs folder to exist)
# This ensures cook compiles with empty assets, forcing rebuild during actual build
RUN mkdir -p crates/client/dashboard/frontend/static

# Build dependencies - THIS LAYER IS CACHED until Cargo.toml/Cargo.lock change
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo chef cook --profile $PROFILE --recipe-path recipe.json

# Now copy source and build (only your code compiles here)
COPY . .
# Force dashboard crate rebuild to ensure rust_embed picks up frontend assets
# cargo-chef cook creates skeleton sources - we must nuke all dashboard artifacts
RUN rm -rf target/*/deps/*dashboard* target/*/build/base-dashboard* target/*/.fingerprint/base-dashboard*
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --profile $PROFILE --bin base-builder && \
    cp /app/target/$([ "$PROFILE" = "dev" ] && echo debug || echo $PROFILE)/base-builder /app/base-builder

# --- Runtime image ---
FROM ubuntu:24.04
RUN apt-get update && \
    apt-get install -y --no-install-recommends jq curl && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/base-builder ./
ENTRYPOINT ["./base-builder"]