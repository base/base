# `docker`

This directory contains the Dockerfiles and Compose configuration for the Base node.

## Dockerfiles

`Dockerfile.rust-services` is the shared multi-target Dockerfile for the Debian-based Rust services. The local devnet builds the unified `base` image for L2 bootnode, sequencer, and validator/RPC nodes.

`Dockerfile.devnet` builds a utility image containing genesis generation tools (`eth-genesis-state-generator`, `eth2-val-tools`, `op-deployer`) and setup scripts. This image bootstraps L1 and L2 chain configurations for local development.

`Dockerfile.nitro-enclave` and `Dockerfile.proxyd` remain separate because they have different toolchains and runtime requirements.

## Docker Compose

The `docker-compose.yml` orchestrates a complete local devnet environment with both L1 and L2 chains. It spins up:

- An L1 execution client (Reth) and consensus client (Lighthouse) with a validator
- Unified Base sequencer and validator/RPC nodes on L2
- The `base-batcher` for submitting L2 data to L1
- The `base-prover-service` JSON-RPC coordinator with local Postgres storage
- The `base-prover-zk-host` worker in `ZK_BACKEND=dry_run` mode

All services read configuration from `devnet-env` in this directory. The devnet stores chain data in `.devnet/` which is created on first run.

## Usage

The easiest way to interact with Docker is through the Justfile recipes:

```bash
just devnet up     # Start fresh devnet (stops existing, clears data, rebuilds)
just devnet up-fast # Start fresh single-sequencer devnet with preallocated L1 rollup state and no ZK prover
just devnet down   # Stop devnet and remove data
just devnet logs   # Stream logs from all containers
just devnet status # Check block numbers and sync status
```

`up-fast` is optimized for local iteration. It uses `op-deployer` in genesis mode
to preallocate the L1 rollup contracts into the L1 genesis state, instead of
deploying those contracts through live L1 transactions after the chain starts.
It also skips the ZK prover image and containers by default. To include the ZK
prover, pass `zk=dry-run` or `zk=cluster`.

Use `up-fast` when you need a quick devnet for node, RPC, batcher, transaction,
or load-test iteration. Use `up` or `up-single` when you need to exercise the
live L1 deployment path, deployment transaction behavior, the HA conductor
setup, or the default ZK prover stack.

To build a specific Rust service image directly:

```bash
just devnet build-image base release
```

Plain `docker build` still works if you prefer it:

```bash
docker build -t base -f etc/docker/Dockerfile.rust-services --target base .
```
