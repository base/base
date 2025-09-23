FROM rust:1-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY .sqlx/ ./.sqlx/

RUN cargo build --bin tips-ingress-writer

FROM debian:bookworm

RUN apt-get update && apt-get install -y libssl3 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/debug/tips-ingress-writer /app/tips-ingress-writer

CMD ["/app/tips-ingress-writer"]