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
mines real descendants of the captured snapshot head and the follow client canonicalizes those
blocks. Transactions submitted to the builder use its real transaction pool and normal
`eth_sendRawTransaction` path.

Interactive `base-devnet` runs the sequencer and validator concurrently.

This is an unsafe-chain development network, not a valid restartable continuation of Base mainnet.
It has no L1, derivation, batching, or safe/finalized-head advancement. At 200ms it produces full
canonical blocks with Base/Reth's standard payload service. The 2s case uses the Flashblocks payload
service, but the 200ms case neither starts nor subscribes to Flashblocks. Treat 200ms results as
full-block results and do not compare Flashblock latency against the 2s case.

## Prerequisites

Run commands from the `base/base` repository root. You need:

- the normal Rust and native build dependencies for this repository;
- an immutable Base Reth snapshot;
- two fresh, writable datadirs restored from that snapshot for each run;
- enough free space for both datadirs to change during the run; and
- Foundry's `cast` only when manually interacting with `base-devnet`.

Each supplied datadir must already exist and contain `db/mdbx.dat`. Builder and client paths must
be distinct. The launcher never creates, copies, deletes, or takes ownership of these datadirs.

For an interactive `base-devnet` session, generate a throwaway key whose address can be prefunded:

```bash
cast wallet new
export FUNDER_KEY=0x... # private key printed above; use only for this local network
cast wallet address --private-key "$FUNDER_KEY"
```

Never use a key that controls real funds.

## Prepare writable snapshot restores

Keep the source snapshot immutable. Use the environment's snapshot/restore mechanism to
materialize two writable datadirs from that same snapshot, then pass their paths to the launcher:

```bash
export BUILDER_DATADIR=/path/to/restored/builder
export CLIENT_DATADIR=/path/to/restored/client
test -f "$BUILDER_DATADIR/db/mdbx.dat"
test -f "$CLIENT_DATADIR/db/mdbx.dat"
```

Snapshot and datadir lifecycle is intentionally outside `base-devnet` so the command cannot destroy
caller-owned data. A long-lived environment may reuse mutated datadirs only when it records their
starting heads. Use fresh equivalent restores when strict boundary equivalence matters.

## Run a snapshot-backed development network

First derive the address for the throwaway funder key. Then start a 2s network and mint funds to
that address in the first local descendant:

```bash
export FUNDER_ADDRESS=$(cast wallet address --private-key "$FUNDER_KEY")

cargo run -p base-system-tests --bin base-devnet -- snapshot \
  --chain sepolia \
  --builder-datadir "$BUILDER_DATADIR" \
  --client-datadir "$CLIENT_DATADIR" \
  --block-interval 2s \
  --prefund-address "$FUNDER_ADDRESS" \
  --runtime-file /tmp/base-snapshot-runtime.json
```

Use `--block-interval 200ms` for the subsecond variant. The first descendant activates `BaseTime`
metadata and subsequent blocks advance on a deterministic 200ms schedule. Snapshot devnets default
to a 10 Ggas block limit at 2s and a 1 Ggas block limit at 200ms, preserving 5 Ggas/s of theoretical
capacity at either cadence. Pass `--block-gas-limit <gas>` to override the cadence default.

Startup validates the selected chain ID, the boundary L1-info transaction, `SystemConfig`, and
sequence number. It waits for the builder to extend the snapshot and for the client to follow before
writing the runtime file. The process then runs until Ctrl-C and shuts both EL runtimes down
gracefully. `--chain` accepts built-in aliases such as `mainnet` and `sepolia`, or a Base genesis
JSON path. A custom genesis whose chain ID is not built in also needs `--rollup-config <rollup.json>`.

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
`block_interval_ms`, `block_gas_limit`, `builder_rpc_url`, `builder_flashblocks_url`, and
`client_rpc_url`. Dynamic ports are the default and are safest for automation. `--stable-ports`
binds the builder and client RPCs to ports 7545 and 8545, respectively, but fails if those ports are
occupied.

To pin a run to a known snapshot boundary, pass all three of `--expected-head-number`,
`--expected-head-hash`, and `--expected-head-timestamp`. Startup fails before load generation if
the captured boundary differs.

## Run a snapshot benchmark

## Run a quick local transfer benchmark

For a no-configuration smoke benchmark, `base-bench` starts a fresh temporary
local devnet, runs the default 60-second plain-transfer profile, prints its
load-test summary, and shuts everything down:

```bash
just devnet bench
```

It needs Docker, but it needs neither a snapshot nor a funded key. The generated
datadirs are temporary and are removed during shutdown. Use the explicit
`base-bench snapshot` arguments below for reproducible snapshot benchmarks and
report artifacts.

## Run the fresh-devnet workload suite

The checked-in fresh-devnet suite runs every workload on a distinct empty
devnet, so token state, accounts, the transaction pool, and caches cannot leak
between scenarios. It currently covers B-20 transfers (with the B-20 asset
feature activated before setup), high-concurrency ETH transfers to new and existing recipients, and a
50,000-round Blake2f precompile profile:

```sh
cargo run --release -p base-system-tests --bin base-bench -- local \
  --workload-config etc/benchmarks/fresh-devnet.yml \
  --output-dir results/fresh-devnet \
  --client-version "base/$(git rev-parse --short HEAD)"
```

The command writes one native load-test sidecar per workload plus a top-level
visualizer manifest:

```text
results/fresh-devnet/
├── metadata.json
├── suite-results.json
├── fresh-devnet-b20-transfer/load-test-result.json
├── fresh-devnet-eth-new/load-test-result.json
├── fresh-devnet-eth-existing/load-test-result.json
└── fresh-devnet-blake2f-50000/load-test-result.json
```

`metadata.json` and the load-test sidecars are directly consumable by the
static visualizer in `base/benchmark`; link this output directory to that
repository's ignored `output/` directory and run its normal production build.
Swap workloads can opt into fresh-devnet contract provisioning with
`deploy_devnet_swap_harness: true` on a workload entry. That mode deploys a
fresh devnet USDC token plus Uniswap/Aerodrome router shims for each workload,
then auto-wires swap and real-token setup addresses before execution.

`base-bench snapshot` owns the process lifecycle around one load test: it generates an ephemeral
funder, deposits funds to it in the first local descendant, replaces placeholder endpoints in the
YAML with dynamically allocated builder endpoints, runs the load generator, writes JSON, and shuts
the stack down. It does not own the caller-provided snapshot datadirs.

For the end-to-end workflow, including disposable snapshot restores, report artifact conventions, and
how `--benchmark-run`, `--scenario`, and `--run-id` select runs in `base/benchmark`, see
[Snapshot Benchmarking](../../docs/guides/SNAPSHOT_BENCHMARKS.md).

Always use an optimized build for performance measurements. Debug payload execution becomes
CPU-bound far below the 400M block gas limit.

```bash
mkdir -p results
export BASE_BENCH_CLIENT_VERSION="base/v0.0.0-$(git rev-parse --short HEAD)"

just devnet bench snapshot \
  --chain mainnet \
  --builder-datadir "$BUILDER_DATADIR" \
  --client-datadir "$CLIENT_DATADIR" \
  --load-test-config \
    crates/infra/load-tests/examples/account-create-mainnet-snapshot.yaml \
  --benchmark-run snapshot-throughput \
  --scenario account-create-2s \
  --output-dir results/account-create-2s
```

The account-create workload uses the adaptive open-loop load generator. Every successful transfer
targets a runtime-random fresh address, forcing an account-trie insertion. Its 100 senders and
1,024 in-flight transactions per sender can hold more than five 400M-gas blocks of 21K-gas
transfers. The cross-cadence example keeps 1M gas outstanding. Larger 20M and 80M fresh-account
queues overran payload deadlines in local storage: the 2s Flashblocks builder missed subsequent FCUs
and the 200ms standard builder could remain inside state-root construction without advancing the
measurement. It measures exactly 500 newly observed canonical blocks. Setup, prefill, and post-run
confirmation draining are outside the measured block count. Use `duration` instead of (or in
addition to) `measurement_blocks` in a custom YAML when a time-bounded smoke test is preferable;
when both are set, the first limit reached stops submissions.

The result includes cadence, boundary number/hash, builder/client endpoints, the generated funder
address (never its private key), explicit measurement boundaries, every measured block's
hash/timestamp/gas/transaction count, phase-specific Prometheus diagnostics, and the native
`MetricsSummary`. The complete sequencer range is measured first; sequencing then stops and the
validator replays that range. A run fails if either role lacks a sample for a measured block or if
their canonical hashes differ. When metrics are available for another load-test failure, they are
written before the command returns the error.

`--output-dir` is one self-contained `base/benchmark` run directory. It receives
`benchmark-result.json`, `metadata.json`, `metrics-sequencer.json`, `metrics-validator.json`, and
`load-test-result.json`. Set `BASE_BENCH_CLIENT_VERSION` to a stable
build label when the report should compare commits or releases. The role metrics contain
`gas/per_block`, `gas/per_second`, `transactions/per_block`, `transactions/per_second`, and selected
Reth Prometheus diagnostics. `BlockNumber` uses two-second-equivalent measurement units: each 2s
block advances by `1.0`, while each 200ms block advances by `0.1`. This aligns equal-duration
cadence runs on report x-axes; canonical block numbers remain in `load-test-result.json`.
`benchmark/prometheus_blocks_per_scrape = 1` identifies
an exact per-block scrape. Values above one mean a fast sequencer advanced multiple blocks during a
scrape: counter deltas are evenly attributed across those blocks, gauges are repeated, and
histogram averages describe the whole scrape interval. Collection is intentionally limited to one
scrape per second because continuously rendering Reth's full endpoint measurably perturbs 200ms
production. Each output directory is one self-contained report run. Give comparable invocations the
same `--benchmark-run` cohort, a descriptive `--scenario`, and a unique `--output-dir`. Report
series are identified by scenario and node role; `--run-id` is only the unique artifact identity and
defaults to `<benchmark-run>-<timestamp>` when omitted. Upload the metrics and artifact files before
`metadata.json` when publishing to the report service because metadata is its completion signal.

For a saturated Blake2f comparison with equal 60-second measured windows, provide separate 2s and
200ms YAMLs with 30 and 300 measured blocks, respectively. Both should issue one fixed
50,000-round Blake2f call per transaction with identical sender, in-flight, batching, funding, and
seed settings. The report labels these runs with `TransactionPayload=blake2f`; use scenarios such
as `blake2f-2s-run-1` and `blake2f-200ms-run-1` to distinguish repetitions. A
5,000-transaction global in-flight cap is exactly one 400M-gas block of queue headroom at the
configured 80,000 gas limit per transaction, which keeps the builder saturated without leaving an
oversized submission backlog at cutoff.

## Compare 2s and 200ms fairly

One restored datadir pair is one run. A run mutates both datadirs and they are not restartable. For every
repetition:

1. Restore builder and client datadirs from the exact same immutable snapshot.
2. Use the same optimized binary, machine, workload settings, measured duration, and funder
   generation method. Scale the block count with cadence (for example, 30 blocks at 2s and 300 at
   200ms for equal 60-second windows).
3. Run one cadence and save its result plus host/build/storage metadata.
4. Stop the stack and remove or reset only that run's disposable datadirs.
5. Restore another fresh pair before running the other cadence.
6. Repeat in alternating order (`2s`, `200ms`, then `200ms`, `2s`) to reduce thermal and cache-order
   bias.

Keep fixed-offered-load comparisons separate from maximum-throughput tuning. Also choose and record
a warm-cache or cold-cache policy; equivalent restores alone do not make page-cache state equal.

## Cleanup

After Ctrl-C or benchmark completion, verify that no process still has either datadir open, then
remove or reset only the disposable datadirs using the environment's snapshot/restore workflow.

Never remove or modify the immutable source snapshot. If shutdown was interrupted, check for a
lingering `base-devnet` or `base-bench` process before changing the datadir lifecycle.

## Troubleshooting

- **Missing `db/mdbx.dat`:** pass the Reth datadir root, not its `db` directory or a parent directory
  that contains another nested datadir.
- **Same builder/client path:** restore two datadirs. One database cannot safely serve both roles.
- **Boundary mismatch:** restore both datadirs from the intended immutable snapshot or correct all
  three expected-head flags; do not partially relax boundary validation.
- **Address delegation or funding errors:** prefund a newly generated throwaway address via
  `--prefund-address` instead of the standard Anvil development address, whose delegated-account
  state at the tested snapshot can trip Reth's delegated-account in-flight limit while funding
  senders.
- **Low gas usage in a debug run:** rerun with `cargo run --release`; debug runs are functional
  smoke tests, not performance evidence.
- **Output write failure:** create the output's parent directory before starting the run.
- **Port conflict:** omit `--stable-ports` and consume the allocated URLs from runtime JSON.
- **Unexpected disk growth:** account creation changes state heavily. Monitor available storage
  throughout long runs.
- **No 200ms Flashblock data:** expected. The 200ms snapshot stack uses Base/Reth's standard payload
  service and does not publish or subscribe to Flashblocks; the 2s path remains unchanged. Compare
  canonical blocks, confirmations, gas, and throughput instead.

See the exact supported options at any revision with:

```bash
cargo run -p base-system-tests --bin base-devnet -- snapshot --help
cargo run -p base-system-tests --bin base-bench -- snapshot --help
```

## Native acceptance tests

The acceptance suite uses ordinary Rust tests and nextest, not a separate scenario registry or
runner. Real containerized L1 execution and consensus clients feed production Base L2 services
running in-process. The current scenarios qualify blob safe derivation and the Amsterdam/Gloas
transition. An observed safe L2 head is not a finalized L2 head.

### Discover and run

```bash
# Compile and list all tests without starting Docker containers or pulling images.
just devnet acceptance-list

# Qualify all helper tests and real-client scenarios in one JUnit report.
just devnet acceptance

# List or run one scenario using the same native nextest filter.
just devnet acceptance-list -E 'test(=blob_derivation::blob_transfer_is_safely_derived)'
just devnet acceptance -E 'test(=blob_derivation::blob_transfer_is_safely_derived)'

# Run only fast helper tests, without provisioning images or containers.
RUST_MIN_STACK=33554432 cargo nextest run --locked --profile acceptance \
  -p base-system-tests --no-default-features --test acceptance
```

The full command requires Docker, Rust, `cargo-nextest`, and `jq`, but no local Python. It builds
`devnet-setup:local-v2` and pulls the digest-pinned Reth/Lighthouse images declared in `fixtures/*.json`.
Listing still needs the repository's native compilation dependencies. A filter that matches no tests
fails the run. Scenarios are ignored by ordinary `cargo test`/nextest runs, but the acceptance command
includes both ignored scenarios and non-ignored helper tests. Tests run serially with zero retries and
a 15-minute per-test ceiling. CI also has a 95-minute aggregate command limit, so suite growth must
account for total runtime rather than assuming every test receives its full individual allowance.

### Add a scenario

1. Add a file under `tests/acceptance/` and declare its module in `tests/acceptance/main.rs`.
   Cargo already discovers that integration-test binary; no manifest or registry entry is needed.
2. Write a `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]` with an explanatory
   `#[ignore = "requires ... and Docker"]` attribute.
3. Call `Acceptance::run` with a unique descriptive name, a fixture factory such as
   `GlamsterdamFixture::builder`, and an async closure receiving `rpc` and `system`.
4. Put actions and behavior-specific assertions inside that closure. Reuse `Transfer`, `Submissions`,
   and the bounded RPC helpers where their contracts fit. Keep hardfork-specific policy in its own
   scenario, not in the lifecycle harness.

[The blob smoke test](tests/acceptance/blob_derivation.rs) is a complete authoring example.
[The fork qualification](tests/acceptance/glamsterdam.rs) demonstrates the same lifecycle with
additional boundary and finality assertions. Both currently choose the pinned Glamsterdam fixture;
the smoke test does not wait for its scheduled fork. Each test gets a fresh stack and artifact
directory; there is no shared running network between scenarios.

The harness catches setup/scenario/diagnostic/shutdown panics, collects independent failures, and
retains the artifact location in errors. It captures diagnostics before shutdown. Component shutdown
deadlines apply without a shorter test-level drain timeout. Dropping or killing an in-flight test
cannot perform async graceful shutdown; ownership-based drop and the command's cleanup are fallbacks.

A new hardfork should provide a dedicated-L1 fixture factory returning `SystemTestStackBuilder`,
using `with_prepared_l1` for its generated artifacts and client configuration. Fixture factories must
preserve startup logs, including when setup fails. Add immutable client pins to `fixtures/` using the
existing `reth.image` and `lighthouse.image` structure. Do not add a DA-mode matrix or a custom runner.

### Artifacts, cleanup, and CI

Set `BASE_ACCEPTANCE_ARTIFACTS` to a fresh path, or let the command allocate a temporary directory.
Each scenario writes `acceptance-<scenario>-<unique>/` beneath it. The fixture records only its own
uniquely named `acceptance-setup-*` and `acceptance-l1-*` networks in `networks` **before** starting
containers. Startup logs, final Docker logs, and inspect output belong in `diagnostics/` and must be
host-readable. Generated configs use `el/*.json`, `l2/*.json`, and `cl/{config.yaml,*.json,genesis.ssz}`.
CI exports these and `target/nextest/acceptance/test-results.xml`, not root-owned validator runtime data.

Normal exit removes owned containers. The command's exit trap and CI's always-run cleanup also remove
running or stopped containers on recorded networks after test failure or timeout. If the entire runner
is killed or Docker is unavailable, cleanup is not guaranteed. Recover with
`just --justfile etc/docker/Justfile _cleanup-acceptance <artifact-directory>`. Cleanup removes only
containers attached to the validated acceptance network names recorded there, not a global Docker
prune. Treat these manifests as ownership records and do not attach unrelated workloads to those networks.

The advisory workflow runs on acceptance/system-harness, fixture, runner, toolchain, workspace
manifest/lockfile, and workflow/setup changes. It also supports manual dispatch. It is not a required
merge gate and does not automatically qualify every production Rust change; manually dispatch it when
a change outside those paths warrants real-client acceptance coverage.
