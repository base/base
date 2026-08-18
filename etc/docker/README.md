# `docker`

This directory contains the Dockerfiles and Compose configuration for the **local devnet** and internal Rust services.

The public operator image (`ghcr.io/base/node`) is the `base` target in `Dockerfile.rust-services`. Published images and operator `--build` use `PROFILE=maxperf` (same as `base/node`). The bake/Dockerfile default stays `release`; `just devnet` builds `dev`. Operator entrypoints live in `etc/scripts/node/`; operators edit `.env.mainnet` / `.env.sepolia` at the repo root. Root `docker-compose.yml` pulls the published image, or compiles this tree with `--build`. `just devnet up` overrides the entrypoint to `./base`.

## Dockerfiles

`Dockerfile.rust-services` is the shared multi-target Dockerfile for the Debian-based Rust services. The `base` target is published as `ghcr.io/base/node` and is also the local devnet image. Devnet compose overrides the default supervisord CMD.

`Dockerfile.devnet` builds a utility image containing genesis generation tools (`eth-genesis-state-generator`, `eth2-val-tools`, `op-deployer`) and setup scripts. This image bootstraps L1 and L2 chain configurations for local development.

`Dockerfile.nitro-enclave` and `Dockerfile.proxyd` remain separate because they have different toolchains and runtime requirements.

## Docker Compose

The `docker-compose.yml` orchestrates a complete local devnet environment with both L1 and L2 chains. It spins up:

- An L1 execution client (Reth) and consensus client (Lighthouse) with a validator
- Unified Base sequencer and validator/RPC nodes on L2
- The `base-batcher` for submitting L2 data to L1

All services read configuration from `devnet-env` in this directory. The devnet stores chain data in `.devnet/` which is created on first run.

`docker-compose.prover.yml` is a separate standalone stack that runs the prover
trio (Postgres, `base-prover-service`, `base-prover-zk-host`) against
user-provided RPC endpoints — including a running devnet's. Run it as
`just prover up <network>` so jobs and Postgres data stay isolated per network;
see the `just prover` recipes and
[docs/guides/STANDALONE_PROVING.md](../../docs/guides/STANDALONE_PROVING.md).

## Usage

The easiest way to interact with Docker is through the Justfile recipes:

```bash
just devnet up     # Start fresh devnet (stops existing, clears data, rebuilds)
just devnet down   # Stop devnet and remove data
just devnet logs   # Stream logs from all containers
just devnet status # Check block numbers and sync status
```

### Single-Anvil L1 no-Nitro proving

The optional single-Anvil variant replaces the Reth execution node and both
Lighthouse processes with one Base-Anvil process. It keeps the L2 and batcher
unchanged, and uses the same Anvil endpoint for L1 execution, Beacon blob
fetching, proof inputs, and proof contracts.

First build the latest Base-Anvil default branch, then start the complete stack:

```bash
just devnet build-anvil-image
just devnet up-anvil-no-nitro
```

The second command generates the L2 genesis, computes its output root offline,
then clones the latest base/contracts default branch and deploys the development
no-Nitro contracts before any L2 node starts. The Base nodes and proof verifier
therefore use the same real `ProtocolVersions` contract from genesis; this path
does not deploy the normal devnet's mock upgrade-signal contract. It then starts
a fresh prover database, registers and starts two Nitro workers in local mode,
and starts the proposer. Inspect or stop it with:

```bash
just anvil-no-nitro status
just anvil-no-nitro logs
just devnet down-anvil
```

Anvil mines one block every four seconds. Do not use timestamp-warp RPCs in
this variant: Base derives Beacon slots from L1 timestamps, so arbitrary time
jumps would break the one-slot-per-execution-block mapping used to fetch blobs.

Denim is activated at block 23 by default, switching the sequencer to its 200ms cadence. To
start a pre-Denim devnet, set `L2_BASE_DENIM_BLOCK` to an empty value:

```bash
L2_BASE_DENIM_BLOCK= just devnet up
```

Zenith is the permanently unscheduled, genesis-only gate for future hardfork feature testing.
To additionally activate it at block 50 (after Denim, so it does not conflict with earlier
activations), start with:

```bash
just devnet up zenith
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
