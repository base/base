# `docker`

This directory contains the Dockerfiles and Compose configuration for the **local devnet** and internal Rust services.

The public operator image (`ghcr.io/base/node`) is the `base` target in `Dockerfile.rust-services`. Published images and operator `--build` use `PROFILE=release` (same as `base/node`), while `just devnet` builds `dev`. `PROFILE` is set on the shared `_rust-service-common` target, so passing it as an environment variable applies it to every target in the invocation; to give one target a different profile, override just that target's build arg — `docker buildx bake -f etc/docker/docker-bake.hcl builder consensus --set builder.args.PROFILE=release-symbols --load` builds `builder` with profiling symbols while `consensus` stays on the default `release`. Operator entrypoints live in `etc/scripts/node/`; operators edit `.env.mainnet` / `.env.sepolia` at the repo root. Root `docker-compose.yml` pulls the published image, or compiles this tree with `--build`. `just devnet up` overrides the entrypoint to `./base`.

## Dockerfiles

`Dockerfile.rust-services` is the shared multi-target Dockerfile for the Debian-based Rust services. The `base` target is published as `ghcr.io/base/node`. The `base-devnet` target extends it with pinned contract artifacts for local development. Devnet compose overrides the default supervisord CMD.

The `setup-devnet` service runs the hidden `/app/base genesis` command from the same `base-devnet:local` image as the nodes and Rust batcher. There is no separate setup image, binary, or runtime generator toolchain.

The devnet image contains its contracts at `/opt/base/contracts`; ordinary startup
needs no host artifact preparation or mount. Operator image builds remain independent
of the contracts stage. Host-side Rust tests use `just contracts` and its shared
content-checked cache; `just contracts-rebuild` forces a new export.

`Dockerfile.op-batcher` builds Go `op-batcher/v1.16.5` at commit
[`abe047af`](https://github.com/ethereum-optimism/optimism/commit/abe047afc995e0e22abf5ea9b157e267e907d494),
matching the Base mainnet infrastructure source pin. The `devnet` and `ingress`
Bake groups build it as `op-batcher:local`; it runs as the canonical batcher.
The Rust `base batcher` (the `base batcher` subcommand of `base-devnet:local`) runs
alongside it in shadow mode; no separate Rust batcher image is needed.

Genesis generation is implemented in `base-genesis`: Revm executes the pinned Solidity
artifacts in-process, and Lighthouse libraries generate the Fulu beacon state and
validator keystore. No Go, Forge, Anvil, or external key-generation process is used.
Rust system tests use the same native generator before starting L1.

Setup retains the existing flags, environment inputs, file paths, and nested L2
mountpoint. Completed outputs are checksummed and reused only for matching inputs.
Legacy, incomplete, or modified configurations require regeneration with
`just devnet down` followed by `just devnet up`; the generator never deletes node
datadirs. System tests call the same library directly before starting L1.

The normal devnet initializes Base's real `ProtocolVersions` registry with the
genesis upgrade schedule. The minimum version defaults to `4294967296` and honors
`UPGRADE_SIGNAL_MIN_PROTOCOL_VERSION`; live ownership, ordering, and notice/freeze
guards remain enforced. Final genesis hashes are written into both rollup
configurations. Nitro disables the generated signal environment because its
bootstrap supplies its own registry; system tests can deploy their own runtime mock.

HA conductor setup also uses `base-devnet:local`, with its helper script mounted read-only.
It uses Bash, curl, and jq to wait for an elected leader, reject JSON-RPC errors,
and verify all three Raft voters before declaring success; no additional image is built.

`Dockerfile.nitro-enclave` and `Dockerfile.proxyd` remain separate because they have different toolchains and runtime requirements.

## Docker Compose

The `docker-compose.yml` orchestrates a complete local devnet environment with both L1 and L2 chains. It spins up:

- An L1 execution client (Reth) and consensus client (Lighthouse) with a validator
- Unified Base sequencer and validator/RPC nodes on L2
- The canonical Go `op-batcher` (`op-batcher` service) submitting L2 data to L1
- The Rust `base batcher` (`base-batcher` service) running in **shadow mode**
- A shadow validator (`base-shadow-validator` service) deriving the shadow DA

All services read configuration from `devnet-env` in this directory. The devnet stores chain data in `.devnet/` which is created on first run.

### Batcher topology (canonical + shadow)

The devnet runs two batchers at once by default, mirroring our internal zeronet
infrastructure:

- **Canonical DA — `op-batcher`.** The Go op-batcher posts to the chain's real
  batch inbox from `BATCHER_ADDR`. `base-client` and `base-rpc` derive from it,
  so this is the chain of record. Single-sequencer and HA devnets explicitly use
  SingularBatch (`--batch-type=0`, Base's SingleBatch format), blob DA, and
  Brotli compression, retaining local low-latency submission settings rather than
  mainnet's runtime config. `--txmgr.cell-proof-time=0` enables Fusaka cell
  proofs from genesis for the local L1 (chain ID 1337), which this version does
  not auto-detect.
- **Shadow DA — `base-batcher`.** The Rust `base batcher` runs in `--shadow-mode`,
  posting to `SHADOW_BATCH_INBOX_ADDRESS` from `SHADOW_BATCHER_ADDR` — a distinct,
  funded dev account, so its L1 nonces never collide with the op-batcher's. It
  anchors recovery on the shadow validator via `--parity-validator-l2-rpc-url`.
- **Shadow validator — `base-shadow-validator`.** A validator-mode `rpc` node
  that overrides the batch inbox and batcher sender
  (`--l1.dangerously-override-da-batch-inbox`,
  `--l1.dangerously-override-da-batcher-sender`) to derive its safe chain from the
  shadow DA instead of the canonical inbox.

Compare the two derived chains for batcher parity with
`etc/scripts/devnet/compare-heads.sh` (point its `CLIENT` endpoint at the shadow
validator's HTTP RPC, `L2_SHADOW_HTTP_PORT`). Prometheus scrapes both batchers
under the `batcher` job (`op-batcher:6060`, `base-batcher:6061`) and the shadow
validator under `l2_shadow_validator`. The Anvil variant inherits all three
services.

**Validity transaction note:** the op-batcher pin embeds op-geth
`v1.101609.2-rc.1`, whose typed block decoder does not support Base's EIP-8130
transaction type `0x79`. A block containing one prevents the canonical op-batcher
from advancing past that block (`transaction type not supported`). The shadow
`base-batcher` supports `0x79`, so the shadow validator continues deriving past
such blocks — useful for validity-transaction testing.

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

Denim activates at block 27 by default and switches the sequencer to its 200ms
cadence. The local Nitro stack exercises proof generation across this boundary:

```bash
just anvil-nitro-local up
```

Set `L2_BASE_DENIM_BLOCK` to another block to move activation, or set it to an
empty value to leave Denim unscheduled.

To exercise validity transactions on the native payload builder, the deployment
must schedule Denim and configure both sides of the forwarding path:

- builder: `--builder.enable-experimental-validity-transactions` and
  `--builder.payload-builder-cutover`. The builder flag also registers
  `base_sendRawTransactionValidity` for direct ingress.
- ingress/client: `--enable-experimental-validity-transactions` and a
  `--builder-rpc-urls` endpoint targeting the builder

The default devnet compose files include these flags and schedule Cobalt at
block 22 and Denim at block 27. Builder selection and block cadence change at
Denim. Native payload building supports balance,
storage, and block-number predicates; `flashblock_index` predicates remain specific
to the Flashblocks builder and are rejected after the Denim cutover.

`just devnet ingress` enables the client's metering RPCs for ingress simulation,
metering-information fan-out, and proxyd routes. The ingress overlay mounts
`resource-metering.json`, a non-empty gas-only dry-run schedule: it observes usage
without adding transaction throttling. Ordinary devnets leave client metering disabled.

Zenith is the permanently unscheduled, genesis-only gate for future hardfork feature testing.
Zenith mode additionally activates Zenith at block 102:

```bash
just devnet up zenith
```

`just devnet up` initializes the real Base `ProtocolVersions` registry in genesis,
writes `.devnet/l2/configs/upgrade-signal.env`, and starts the normal L2 nodes in
`runtime-admin` mode. The devnet image includes artifacts built from the revision in
`etc/upstream-pins/contracts.rev`; no separate setup image is needed. Host-side
generation uses the same export, and system tests provision it on demand unless
`BASE_GENESIS_ARTIFACTS` selects an existing directory.
`just devnet smoke` checks transfers and an L1 portal deposit credited on L2; its
blob test requires an EIP-7594-capable `cast` (tested with 1.7.1).

```bash
just devnet upgrade-signal status
just devnet upgrade-signal set denim 1800000000
```

Updates use the real owner-gated `setTimestamp` API. Only mutable trailing upgrades
can be newly scheduled or cleared; existing activations cannot be moved after their
one-hour freeze window begins. The default devnet activates Denim almost immediately,
so use a fresh devnet with a later Denim block for scheduling experiments.
`upgrade-signal-future denim 3660` chooses a timestamp relative to the later of
L1/L2 heads, allowing the contract's one-hour notice period. Historical genesis
activations are never rewritten by `upgrade-signal setup`.

To observe the L1 schedule without dynamically applying it, start devnet in metrics-only mode:

```bash
UPGRADE_SIGNAL_MODE=metrics-only just devnet up
```

To build a specific Rust service image directly:

```bash
just devnet build-image base-devnet release
```

Plain `docker build` still works if you prefer it:

```bash
docker build -t base-devnet:local -f etc/docker/Dockerfile.rust-services --target base-devnet .
```

Use the `base` target instead to build the operator image without contract artifacts.
