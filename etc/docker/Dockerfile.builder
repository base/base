# --- Builder ---
FROM rust:1.93-trixie AS builder
WORKDIR /app
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      git libclang-dev pkg-config curl build-essential && \
    rm -rf /var/lib/apt/lists/*

ARG PROFILE=release

COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,id=builder-target,sharing=locked \
    cargo build --profile $PROFILE --package base-builder-bin --bin base-builder && \
    cp /app/target/$([ "$PROFILE" = "dev" ] && echo debug || echo $PROFILE)/base-builder /app/base-builder

# --- Runtime ---
FROM debian:trixie-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    useradd -r -s /sbin/nologin app

WORKDIR /app
COPY --from=builder /app/base-builder ./
USER app
ENTRYPOINT ["./base-builder"]
