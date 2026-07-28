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
- The `base-prover-zk-host` worker (dry-run when RPC URLs are set)

The ZK worker's backend is selected at devnet startup: `just devnet up`
defaults to `zk=dry-run`, `zk=cluster` uses a self-hosted SP1 cluster, and
`zk=network` submits paid proof requests to the Succinct Prover Network (it
requires a funded `NETWORK_PRIVATE_KEY` in the environment and real SP1 ELFs
built via `just succinct build-elfs`).

`docker-compose.prover.yml` is a standalone stack that runs only the prover
trio (Postgres, prover-service, zk-host) against user-provided RPC endpoints.
Run it as `just prover up <network>` so jobs and Postgres data stay isolated
per network — see the
[standalone proving guide](../../docs/guides/STANDALONE_PROVING.md) and the
`just prover` recipes.

All services read configuration from `devnet-env` in this directory. The devnet stores chain data in `.devnet/` which is created on first run.

## Usage

The easiest way to interact with Docker is through the Justfile recipes:

```bash
just devnet up     # Start fresh devnet (stops existing, clears data, rebuilds)
just devnet down   # Stop devnet and remove data
just devnet logs   # Stream logs from all containers
just devnet status # Check block numbers and sync status
```

`just devnet up` deploys a local L1 `MockProtocolVersions` contract, writes
`.devnet/l2/configs/upgrade-signal.env`, and starts the normal L2 nodes in
`runtime-admin` upgrade-signal mode. You can inspect or update the live schedule
with:

```bash
just devnet upgrade-signal status
just devnet upgrade-signal set azul 1800000000
just devnet upgrade-signal-future azul 120
```

To observe the L1 schedule without dynamically applying it, start devnet in metrics-only mode:

```bash
UPGRADE_SIGNAL_MODE=metrics-only just devnet up
```

To build a specific Rust service image directly:

```bash
just devnet build-image base release
```

Plain `docker build` still works if you prefer it:

```bash
docker build -t base -f etc/docker/Dockerfile.rust-services --target base .
```
