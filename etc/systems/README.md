# `base-system-tests`

System-test and development-network infrastructure for Base nodes. In addition to the fresh
L1/L2 stack used by system tests, this crate can continue a Base mainnet execution snapshot with
real builder and client execution and consensus components in one managed launcher process.

## Snapshot devnet topology

The snapshot mode starts these real local network roles inside one managed launcher process:

```text
snapshot builder EL <-> standalone L1-free sequencer CL
        |
        | unsafe blocks
        v
 follow-mode CL      <-> snapshot client EL
```

Both ELs start from separate writable copies of the same Base mainnet Reth datadir. The builder
mines real descendants of the captured mainnet head and the follow client canonicalizes those
blocks. Transactions submitted to the builder use its real transaction pool and normal
`eth_sendRawTransaction` path.

Interactive `base-devnet` runs the sequencer and validator concurrently. `base-bench` instead runs
two sequential phases: it first produces the complete measured range with only the sequencer
active, stops sequencing while retaining the builder RPC as the canonical source, then starts the
full follow-mode validator and replays that exact range. Sequential execution isolates
process-global Reth metrics by role and makes builder/validator hash equality an acceptance check.

This is an unsafe-chain benchmark network, not a valid restartable continuation of Base mainnet.
It has no L1, derivation, batching, or safe/finalized-head advancement. At 200ms it produces full
canonical blocks with Base/Reth's standard payload service. The 2s case uses the Flashblocks payload
service, but the 200ms case neither starts nor subscribes to Flashblocks. Treat 200ms results as
full-block results and do not compare Flashblock latency against the 2s case.

## Prerequisites

Run commands from the `base/base` repository root. You need:

- the normal Rust and native build dependencies for this repository;
- an immutable Base mainnet Reth snapshot;
- two fresh, writable clones of that snapshot for each run;
- enough free space for both clones to diverge during the run; and
- Foundry's `cast` only when manually interacting with `base-devnet`.

Each supplied datadir must already exist and contain `db/mdbx.dat`. Builder and client paths must
be distinct. The launcher never creates, copies, deletes, or takes ownership of these datadirs.

`base-bench` always generates an ephemeral funder key for each run and deposits its funds in the
first local descendant, so the funder is never configurable.

For an interactive `base-devnet` session, generate a throwaway key whose address can be prefunded:

```bash
cast wallet new
export FUNDER_KEY=0x... # private key printed above; use only for this local network
cast wallet address --private-key "$FUNDER_KEY"
```

Never use a key that controls real funds.

## Prepare local ZFS clones

Keep the source dataset immutable and clone the same snapshot once for each EL. Substitute your
actual source snapshot and disposable dataset names:

```bash
export SNAPSHOT=zroot/data/snapshots/base-mainnet-06-09@latest
export BUILDER_DATASET=zroot/data/snapshots/base-mainnet-bench-builder
export CLIENT_DATASET=zroot/data/snapshots/base-mainnet-bench-client

sudo zfs clone "$SNAPSHOT" "$BUILDER_DATASET"
sudo zfs clone "$SNAPSHOT" "$CLIENT_DATASET"

export BUILDER_DATADIR=/home/user/snapshots/base-mainnet-bench-builder
export CLIENT_DATADIR=/home/user/snapshots/base-mainnet-bench-client
test -f "$BUILDER_DATADIR/db/mdbx.dat"
test -f "$CLIENT_DATADIR/db/mdbx.dat"
```

An AWS run has the same ownership contract: materialize two writable datadirs from the same
immutable snapshot on instance-local `NVMe`, then pass their paths. Snapshot and volume lifecycle is
intentionally outside `base-devnet` and `base-bench` so neither command can destroy caller-owned
data. The simple workflow keeps one sequencer datadir and one validator datadir for the life of the
instance and reuses them rather than rolling them back between runs; record the mutated starting
heads when doing so. Use fresh equivalent copies when strict boundary equivalence matters.

## Run a snapshot-backed development network

First derive the address for the throwaway funder key. Then start a 2s network and mint funds to
that address in the first local descendant:

```bash
export FUNDER_ADDRESS=$(cast wallet address --private-key "$FUNDER_KEY")

cargo run -p base-system-tests --bin base-devnet -- snapshot \
  --builder-datadir "$BUILDER_DATADIR" \
  --client-datadir "$CLIENT_DATADIR" \
  --block-interval 2s \
  --prefund-address "$FUNDER_ADDRESS" \
  --runtime-file /tmp/base-snapshot-runtime.json
```

Use `--block-interval 200ms` for the subsecond variant. The first descendant activates `BaseTime`
metadata and subsequent blocks advance on a deterministic 200ms schedule.

Startup validates chain ID 8453, the boundary L1-info transaction, `SystemConfig`, and sequence
number. It waits for the builder to extend the snapshot and for the client to follow before writing
the runtime file. The process then runs until Ctrl-C and shuts both EL runtimes down gracefully.

In another terminal, inspect the machine-readable endpoints and compare the live heads:

```bash
jq . /tmp/base-snapshot-runtime.json

BUILDER_RPC=$(jq -r .builder_rpc_url /tmp/base-snapshot-runtime.json)
CLIENT_RPC=$(jq -r .client_rpc_url /tmp/base-snapshot-runtime.json)

cast chain-id --rpc-url "$BUILDER_RPC"
cast block-number --rpc-url "$BUILDER_RPC"
cast block-number --rpc-url "$CLIENT_RPC"
cast balance "$FUNDER_ADDRESS" --rpc-url "$BUILDER_RPC"
```

The runtime JSON contains `status`, `chain_id`, `boundary_number`, `boundary_hash`,
`block_interval_ms`, `builder_rpc_url`, `builder_flashblocks_url`, and `client_rpc_url`. Dynamic
ports are the default and are safest for automation. `--stable-ports` binds the builder and client
RPCs to ports 7545 and 8545, respectively, but fails if those ports are occupied.

To pin a run to a known snapshot boundary, pass all three of `--expected-head-number`,
`--expected-head-hash`, and `--expected-head-timestamp`. Startup fails before load generation if
the captured boundary differs.

## Run the account-creation benchmark

`base-bench snapshot` owns the process lifecycle around one load test: it generates an ephemeral
funder, deposits funds to it in the first local descendant, replaces placeholder endpoints in the
YAML with dynamically allocated builder endpoints, runs the load generator, writes JSON, and shuts
the stack down. It does not own the snapshot clones.

Always use an optimized build for performance measurements. Debug payload execution becomes
CPU-bound far below the 400M block gas limit.

```bash
mkdir -p results
export BASE_BENCH_CLIENT_VERSION="base/v0.0.0-$(git rev-parse --short HEAD)"

cargo run --release -p base-system-tests --bin base-bench -- snapshot \
  --builder-datadir "$BUILDER_DATADIR" \
  --client-datadir "$CLIENT_DATADIR" \
  --block-interval 2s \
  --load-test-config \
    crates/infra/load-tests/examples/account-create-mainnet-snapshot.yaml \
  --benchmark-run snapshot-throughput \
  --output-dir results/account-create-2s
```

The account-create workload uses the adaptive open-loop load generator from base/base PR #4156.
Every successful transfer targets a runtime-random fresh address, forcing an account-trie
insertion. Its 100 senders and 1,024 in-flight transactions per sender can hold more than five
400M-gas blocks of 21K-gas transfers. The cross-cadence example keeps 1M gas outstanding. Larger
20M and 80M fresh-account queues overran payload deadlines on local ZFS: the 2s Flashblocks builder
missed subsequent FCUs and the 200ms standard builder could remain inside state-root construction
without advancing the measurement. It measures exactly 500 newly observed canonical blocks. Setup,
prefill, and post-run confirmation draining are outside the measured block count. Use
`duration` instead of (or in addition to) `measurement_blocks` in a custom YAML when a time-bounded
smoke test is preferable;
when both are set, the first limit reached stops submissions.

The result includes cadence, boundary number/hash, builder/client endpoints, the generated funder
address (never its private key), explicit measurement boundaries, every measured block's
hash/timestamp/gas/transaction count, phase-specific Prometheus diagnostics, and the native
`MetricsSummary`. The complete sequencer range is measured first; sequencing then stops and the
validator replays that range. A run fails if either role lacks a sample for a measured block or if
their canonical hashes differ. When metrics are available for another load-test failure, they are
written before the command returns the error.

`--output-dir` receives `benchmark-result.json` plus a `visualizer/` subdirectory that writes the
`base/benchmark` report contract directly: `metadata.json`, `metrics-sequencer.json`,
`metrics-validator.json`, and `load-test-result.json`. Set `BASE_BENCH_CLIENT_VERSION` to a stable build label when the report
should compare commits or releases. The role metrics contain `gas/per_block`, `gas/per_second`,
`transactions/per_block`, `transactions/per_second`, and selected Reth Prometheus diagnostics.
`BlockNumber` is relative to the measurement (`1..N`); canonical block numbers remain in
`load-test-result.json`. `benchmark/prometheus_blocks_per_scrape = 1` identifies an exact per-block
scrape. Values above one mean a fast sequencer advanced multiple blocks during a scrape: counter
deltas are evenly attributed across those blocks, gauges are repeated, and histogram averages
describe the whole scrape interval. Collection is intentionally limited to one scrape per second
because continuously rendering Reth's full endpoint measurably perturbs 200ms production. Each
output directory is one self-contained report run. Give comparable invocations the same
`--benchmark-run` cohort and a unique `--output-dir`; the run ID defaults to
`<benchmark-run>-<timestamp>` unless `--run-id` is set. Upload
the metrics and artifact files before `metadata.json` when publishing to the report service because
metadata is its completion signal.

## Compare 2s and 200ms fairly

One clone pair is one run. A run mutates both clones and they are not restartable. For every
repetition:

1. Clone builder and client datadirs from the exact same immutable snapshot.
2. Use the same optimized binary, machine, load-test YAML, block count, and funder generation method.
3. Run one cadence and save its result plus host/build/storage metadata.
4. Stop the stack and destroy only that run's disposable clones.
5. Create another fresh pair before running the other cadence.
6. Repeat in alternating order (`2s`, `200ms`, then `200ms`, `2s`) to reduce thermal and cache-order
   bias.

Keep fixed-offered-load comparisons separate from maximum-throughput tuning. Also choose and record
a warm-cache or cold-cache policy; clone equivalence alone does not make page-cache state equal.

## Cleanup

After Ctrl-C or benchmark completion, verify that no process still has either mount open, then
destroy only the disposable datasets:

```bash
sudo zfs destroy "$BUILDER_DATASET"
sudo zfs destroy "$CLIENT_DATASET"
```

Never destroy the immutable source snapshot. If shutdown was interrupted, check for a lingering
`base-devnet` or `base-bench` process before unmounting or destroying storage.

## Troubleshooting

- **Missing `db/mdbx.dat`:** pass the Reth datadir root, not its `db` directory or a ZFS mount that
  contains another nesting level.
- **Same builder/client path:** create two clones. One database cannot safely serve both roles.
- **Boundary mismatch:** recreate both clones from the intended immutable snapshot or correct all
  three expected-head flags; do not partially relax boundary validation.
- **Address delegation or funding errors:** `base-bench` always uses a fresh ephemeral funder. For
  `base-devnet`, prefund a newly generated throwaway address via `--prefund-address` instead of the
  standard Anvil development address, whose delegated-account state at the tested snapshot can trip
  Reth's delegated-account in-flight limit while funding senders.
- **Low gas usage in a debug run:** rerun with `cargo run --release`; debug runs are functional smoke
  tests, not performance evidence.
- **Output write failure:** create the output's parent directory before starting the run.
- **Port conflict:** omit `--stable-ports` and consume the allocated URLs from runtime JSON.
- **Unexpected disk growth:** account creation changes state heavily. Monitor ZFS referenced space
  or EBS free space throughout long runs.
- **No 200ms Flashblock data:** expected. The 200ms snapshot stack uses Base/Reth's standard payload
  service and does not publish or subscribe to Flashblocks; the 2s path remains unchanged. Compare
  canonical blocks, confirmations, gas, and throughput instead.

See the exact supported options at any revision with:

```bash
cargo run -p base-system-tests --bin base-devnet -- snapshot --help
cargo run -p base-system-tests --bin base-bench -- snapshot --help
```
