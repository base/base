# Development Networks

Base has four ways to run a local chain. They differ in whether they run an L1, where L2 state comes
from, and how much runs in Docker. This guide helps you pick one and explains how they share code.
For the benchmarks that run on top of them, see [Benchmarking](BENCHMARKING.md); for how they are
used in tests, see [Testing Overview](TESTING.md).

## Which one do I want?

| Network | Start it with | L1 | L2 state | Docker | Startup | Use it for |
|---|---|---|---|---|---|---|
| **Light devnet** | `base-devnet light` | none | Fresh generated genesis | No | Seconds | Day-to-day development, poking at RPC, quick load tests |
| **Full devnet** | `just devnet up-single` or `just devnet up` | Reth + Lighthouse | Fresh genesis from `op-deployer` | Yes | Minutes | Anything involving the L1: batching, derivation, proposer and prover, upgrade signals, conductor/HA |
| **Snapshot devnet** | `base-devnet snapshot` | none | Your Base mainnet or Sepolia datadir | No | Depends on datadir | Performance against real state size |
| **Test harness** | `just devnet tests`, `base-bench local` | Reth + Lighthouse (testcontainers) | Fresh genesis | Yes | Minutes | System tests and fresh-state TPS benchmarks |

A rule of thumb: start with the light devnet. Move to the full devnet when you need an L1 and the
services that talk to it.

## Light devnet

One process runs a builder execution node and a standalone sequencer. No L1, no beacon node, no
Docker, and nothing leaves localhost.

```bash
cargo run -p base-system-tests --bin base-devnet --no-default-features -- light
```

It prints the chain ID and RPC, WebSocket, and Flashblocks URLs, then runs until Ctrl-C or SIGTERM.
The default chain is generated from the built-in dev chain configuration: the Anvil test accounts are
prefunded, every fork through Cobalt is active at genesis, and blocks come every two seconds.

Useful options (see `base-devnet light --help` for all of them):

| Option | Effect |
|---|---|
| `--block-interval 200ms` | Subsecond blocks using `BaseTime` metadata (Denim activates at the first block) |
| `--prefund-address <addr>` | Mint ETH to an extra address in the first block |
| `--stable-ports` | Bind the standard developer ports (builder HTTP on 7545) instead of free ports |
| `--datadir <dir>` | Keep chain data in a new directory instead of a temporary one removed on exit |
| `--runtime-file <path>` | Write the endpoints as JSON once the chain is advancing |

What it does not have, because there is no L1: safe and finalized heads do not advance, there is no
batcher or derivation, and nothing is posted to a data-availability layer. Use the full devnet for
those. Details are in [`etc/systems/README.md`](../../etc/systems/README.md#light-devnet).

## Full devnet

Compose brings up an L1 (Reth and Lighthouse with a validator), an L2 builder and sequencer, a client
and RPC node, the canonical Go `op-batcher`, and a Rust `base batcher` running in shadow mode. All
services read [`etc/docker/devnet-env`](../../etc/docker/devnet-env) and keep data in `.devnet/`.

```bash
just devnet up-single   # single sequencer
just devnet up          # three-node HA cluster managed by a conductor
just devnet status
just devnet down
```

In `up-single`, the `base-builder` service runs `base-devnet fresh`, which hosts the same in-process
builder and sequencer as the other modes but connects to the Compose-managed L1. The HA topology runs
the integrated `base sequencer` command instead, because conductor manages each sequencer
independently. See [`etc/docker/README.md`](../../etc/docker/README.md) for services, images, and
optional overlays such as the single-Anvil Nitro proving stack and the transaction-observability
stack.

## Snapshot devnet

Continues a real Base datadir with no L1. A builder and a follow-mode client each start from their own
writable restore of the same snapshot, so the chain has production state size. It is an unsafe-chain
network, not a valid continuation of mainnet. See the
[snapshot section of the crate README](../../etc/systems/README.md#snapshot-devnet) and
[Snapshot Benchmarking](SNAPSHOT_BENCHMARKS.md).

## Test harness

System tests start an isolated L1 and L2 with testcontainers and run the builder, sequencer, client,
and batcher in the test process. `base-bench local` provisions the same kind of stack for each
benchmark workload. Both are driven by `L2Stack` in `etc/systems`.

## How they fit together

```text
                  base-devnet (etc/systems)                 base-bench (etc/systems)
          ┌───────────┬────────────┬───────────┐          ┌───────────┬────────────┐
          │  light    │   fresh    │ snapshot  │          │   local   │  snapshot  │
          └─────┬─────┴──────┬─────┴─────┬─────┘          └─────┬─────┴──────┬─────┘
                │            │           │                      │            │
   standalone sequencer  SequencerStack  │                   L2Stack          │
   (no L1) + builder     (needs an L1)   │                   (testcontainers  │
                                         │                    L1 + SequencerStack)
                                         └──── SnapshotL2Stack ◄──────────────┘
```

- `base-devnet` and `base-bench` are two binaries in the `base-system-tests` crate. `base-devnet`
  starts a network and waits; `base-bench` starts one, runs a load test against it, writes results,
  and stops it.
- `SequencerStack` (in-process builder plus a sequencer that follows an L1) is shared by
  `base-devnet fresh`, `L2Stack`, and so by the system tests and `base-bench local`.
- The light and snapshot devnets both use the standalone, L1-free sequencer; they differ in where the
  L2 state comes from (generated genesis versus a datadir).
- `just devnet up-single` is the only place `base-devnet fresh` is used by a developer workflow.

### Names that get confused

- **`base-devnet fresh`** is the launcher mode the Docker devnet uses for its builder. It needs an L1
  and is not the light devnet.
- **"Fresh devnet"** in `base-bench local` and the workload suite means a newly provisioned, empty
  test-harness stack per workload. It needs Docker for its L1.
- **Light devnet** is `base-devnet light`: fresh state and no L1.
