FROM rust:1-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN cargo build --bin tips-audit

FROM debian:bookworm

RUN apt-get update && apt-get install -y libssl3 ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/debug/tips-audit /app/tips-audit

EXPOSE 3001

CMD ["/app/tips-audit"]