FROM rust:1.85 AS build

WORKDIR /app

RUN apt-get update && apt-get -y upgrade && apt-get install -y git libclang-dev pkg-config curl build-essential

COPY ./ .

RUN cargo build --bin base-reth-node

FROM ubuntu:22.04

RUN apt-get update && \
    apt-get install -y jq curl && \
    rm -rf /var/lib/apt/lists

WORKDIR /app

COPY --from=build /app/target/debug/base-reth-node ./