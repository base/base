# `docker`

This directory contains the Dockerfiles and Compose configuration for the **local devnet** and internal Rust services.

The public operator image (`ghcr.io/base/node`) is the `base` target in `Dockerfile.rust-services`. Published images and operator `--build` use `PROFILE=release` (same as `base/node`), while `just devnet` builds `dev`. `PROFILE` is set on the shared `_rust-service-common` target, so passing it as an environment variable applies it to every target in the invocation; to give one target a different profile, override just that target's build arg — `docker buildx bake -f etc/docker/docker-bake.hcl builder consensus --set builder.args.PROFILE=release-symbols --load` builds `builder` with profiling symbols while `consensus` stays on the default `release`. Operator entrypoints live in `etc/scripts/node/`; operators edit `.env.mainnet` / `.env.sepolia` at the repo root. Root `docker-compose.yml` pulls the published image, or compiles this tree with `--build`. `just devnet up` overrides the entrypoint to `./base`.

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

### Single-Anvil L1 local Nitro proving

The optional single-Anvil variant replaces the Reth execution node and both
Lighthouse processes with one Base-Anvil process. It keeps the L2 and batcher
unchanged, and uses the same Anvil endpoint for L1 execution, Beacon blob
fetching, proof inputs, and proof contracts.

First build the latest Base-Anvil default branch, then start the complete stack:

```bash
just devnet build-anvil-image
just anvil-nitro-local up
```

The second command generates the L2 genesis, computes its output root offline,
then clones the latest base/contracts default branch and deploys the development
no-Nitro contracts before any L2 node starts. These contracts bypass hardware
attestation, while the workers run the Nitro enclave proving code in-process.
The Base nodes and proof verifier therefore use the same real `ProtocolVersions`
contract from genesis; this path does not deploy the normal devnet's mock
upgrade-signal contract. Docker Compose then starts a proofs-history execution
node with a follow-mode consensus node, a fresh prover database, prover-service,
two registered Nitro workers in local mode, and the proposer. Inspect or stop it
with:

```bash
just anvil-nitro-local status
just anvil-nitro-local logs
just anvil-nitro-local down
```

Anvil mines one block every 12 seconds. Do not use timestamp-warp RPCs in
this variant: Base derives Beacon slots from L1 timestamps, so arbitrary time
jumps would break the one-slot-per-execution-block mapping used to fetch blobs.

Denim activates at block 25 by default and switches the sequencer to its 200ms
cadence. The local Nitro stack exercises proof generation across this boundary:

```bash
just anvil-nitro-local up
```

Set `L2_BASE_DENIM_BLOCK` to another block to move activation, or set it to an
empty value to run a pre-Denim devnet.

To exercise Cobalt validity transactions on the native payload builder, the
deployment must schedule Cobalt no later than Denim and configure both sides of
the forwarding path:

- builder: `--builder.enable-experimental-validity-transactions` and
  `--builder.payload-builder-cutover`. The builder flag also registers
  `base_sendRawTransactionValidity` for direct ingress.
- ingress/client: `--enable-experimental-validity-transactions` and a
  `--builder-rpc-urls` endpoint targeting the builder

The default devnet compose files include these flags and schedule Cobalt at
block 22 followed by Denim at block 25. Native payload building supports balance,
storage, and block-number predicates; `flashblock_index` predicates remain
specific to the Flashblocks builder and are rejected after the Denim cutover.

Zenith is the permanently unscheduled, genesis-only gate for future hardfork feature testing.
Zenith mode additionally activates Zenith at block 100:

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
