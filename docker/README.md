# `docker`

This directory contains the Dockerfiles and Compose configuration for the Base node.

## Dockerfiles

`Dockerfile.client` builds the `base-reth-node` binary, the production OP Stack execution client. This is the primary image used for running Base nodes and is what gets published to the container registry on releases.

`Dockerfile.builder` builds the `base-builder` binary, an extended execution client with block building capabilities including flashblocks support. This is used by sequencers and block builders in the network.

`Dockerfile.devnet` builds a utility image containing genesis generation tools (`eth-genesis-state-generator`, `eth2-val-tools`, `op-deployer`) and setup scripts. This image bootstraps L1 and L2 chain configurations for local development.

## Docker Compose

The `docker-compose.yml` orchestrates a complete local devnet environment with both L1 and L2 chains. It spins up:

- An L1 execution client (Reth) and consensus client (Lighthouse) with a validator
- The Base builder and client nodes on L2
- OP Stack consensus layer nodes (`op-node`) for both builder and client
- The `op-batcher` for submitting L2 data to L1

All services read configuration from `.env.devnet` in the repository root. The devnet stores chain data in `.devnet/` which is created on first run.

## Usage

The easiest way to interact with Docker is through the Justfile recipes:

```bash
just devnet        # Start fresh devnet (stops existing, clears data, rebuilds)
just devnet-down   # Stop devnet and remove data
just devnet-logs   # Stream logs from all containers
just devnet-status # Check block numbers and sync status
```

To build the client image directly:

```bash
docker build -t base-reth-node -f docker/Dockerfile.client .
```

To run the compose stack manually:

```bash
docker compose --env-file .env.devnet -f docker/docker-compose.yml up -d --build
```
