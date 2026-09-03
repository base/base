# Snapshot Benchmarking

This guide covers repeatable performance measurements on an L1-free Base
snapshot devnet. It explains how to run `base-bench snapshot`, publish its output
to `base/benchmark`, and select the exact runs to compare.

The snapshot benchmark is for local performance investigation. It mutates its
builder and validator datadirs, so every attempt needs a fresh writable datadir pair.

## Prerequisites

- Build an optimized `base-bench` binary. Debug builds are not suitable for a
  400M-gas performance measurement.
- Start from an immutable, fully prepared Base datadir snapshot.
- Restore that exact snapshot into separate writable builder and validator datadirs.
- Keep the source snapshot immutable for benchmark purposes; only remove or reset
  the disposable per-run datadirs.

Materialize a pair for one run using the snapshot/restore system appropriate to the
environment, then set the datadir paths:

```sh
RUN=blake2f-2s-run-1
BUILDER_DATADIR=/path/to/restored/${RUN}-builder
CLIENT_DATADIR=/path/to/restored/${RUN}-client

test -f "$BUILDER_DATADIR/db/mdbx.dat"
test -f "$CLIENT_DATADIR/db/mdbx.dat"
```

## Run One Benchmark

Build and run the snapshot harness from this repository:

```sh
cargo build --release -p base-system-tests --bin base-bench

export BASE_BENCH_CLIENT_VERSION="base/$(git rev-parse --short HEAD)"

target/release/base-bench snapshot \
  --chain sepolia \
  --builder-datadir "$BUILDER_DATADIR" \
  --client-datadir "$CLIENT_DATADIR" \
  --load-test-config /path/to/blake2f-2s.yaml \
  --benchmark-run snapshot-throughput \
  --scenario blake2f-2s-run-1 \
  --run-id blake2f-2s-<timestamp> \
  --output-dir results/blake2f-2s-run-1
```

`--chain` selects the network rules used to continue the snapshot. It accepts the
built-in Base aliases, including `mainnet` (the default) and `sepolia`. It also
accepts a path to a Base genesis JSON file. A JSON chain whose L2 chain ID is not
one of the built-in Base networks additionally requires its rollup configuration:

```sh
target/release/base-bench snapshot \
  --chain /path/to/genesis.json \
  --rollup-config /path/to/rollup.json \
  --builder-datadir "$BUILDER_DATADIR" \
  --client-datadir "$CLIENT_DATADIR" \
  --load-test-config /path/to/load-test.yaml \
  --benchmark-run custom-throughput \
  --scenario custom-run-1 \
  --output-dir results/custom-run-1
```

For a genesis JSON whose L2 chain ID matches a built-in Base network, the built-in
rollup configuration is selected automatically and `--rollup-config` is optional.

`base-bench snapshot` launches the snapshot builder and validator, funds an
ephemeral load-test account, runs the workload, verifies that both roles have
matching canonical blocks for the measured window, and then shuts down.

Each `--output-dir` is a self-contained `base/benchmark` input directory:

```text
<output-dir>/
  benchmark-result.json
  load-test-result.json
  metadata.json
  metrics-sequencer.json
  metrics-validator.json
```

`metadata.json` is the completion signal when publishing results. Write or upload
the metrics and artifacts before it.

## Aggregate selected runs into one report

Raw result directories are immutable inputs. Build a compact report index without
moving, deleting, or rewriting those artifacts by passing the report root and a
selected list of its direct-child run directories:

```sh
target/release/base-bench aggregate \
  --output-dir /absolute/path/to/results \
  /absolute/path/to/results/run-a \
  /absolute/path/to/results/run-b
```

The command atomically writes `results/metadata.json`. It retains the latest
`createdAt` entry for every unique report tag set. The semantic `Scenario` is
part of that identity, while a final generated ` @ <RFC3339 UTC timestamp>`
suffix is ignored during comparison. This preserves distinct scenarios and
versions, but replaces an older repetition with the latest one. The aggregate
metadata adds the retained run time to `Scenario`, making the report filter and
chart label identify when that result was last measured. `id` remains the unique
raw artifact identifier and `BenchmarkRun` remains the report cohort/page key.

## Choose Comparable Runs

Use the fields below consistently. They have different purposes.

| Field | Purpose | Comparison rule |
|---|---|---|
| `--benchmark-run` | Report cohort | Use the same value for every candidate in one comparison, such as `snapshot-throughput`. |
| `--scenario` | User-visible experiment/repetition label | Use a descriptive value for each selected run, such as `blake2f-2s-run-1` or `blake2f-200ms-run-1`. |
| `--run-id` | Immutable artifact identifier | Keep it unique. It does not group runs. |
| `--output-dir` | Complete local artifact directory | Keep it unique. Never allow two runs to write to the same directory. |
| `BASE_BENCH_CLIENT_VERSION` | Binary/build label | Keep it equal for cadence comparisons; deliberately vary it only for client-version comparisons. |

For a fair 2-second versus 200-millisecond Blake2f comparison on any selected chain:

1. Restore both roles from the same immutable snapshot for every repetition.
2. Use the same release binary, host, load parameters, seed, and funding policy.
3. Configure the 2-second YAML with 30 measured blocks.
4. Configure the 200-millisecond YAML with 300 measured blocks.
5. Give both cases the same `--benchmark-run`; use distinct scenarios and artifact IDs.
6. Alternate cadence order across repetitions (`2s`, `200ms`, then `200ms`, `2s`) to reduce thermal and cache-order bias.

For both YAMLs, use one fixed Blake2f precompile payload with 50,000 rounds and
keep sender count, in-flight limits, batching, funding, and seed identical. The
2-second case uses 30 blocks and the 200-millisecond case uses 300, giving both
the same 60-second measurement window.

Keep maximum-throughput tuning separate from fixed-offered-load comparisons. Record
whether runs use a warm or cold page-cache policy; matching restored datadirs alone
do not make page-cache state equal.

Snapshot roles persist every block so the disposable datadirs remain inspectable. Reth's Engine
persistence-backpressure threshold is cadence-scaled to preserve the default wall-clock headroom:
16 blocks at 2 seconds and 160 blocks at 200 milliseconds. Without this scaling, a slow persistence
flush can stop Engine API intake after only 3.2 seconds at 200 milliseconds, producing empty-block
runs followed by catch-up bursts. The report artifacts retain the Engine backpressure and failed
response metrics so this condition is visible rather than misclassified as workload capacity.

## Compare In Base/benchmark

Place each completed output directory beneath one parent directory, for example:

```text
results/
  blake2f-2s-run-1/
  blake2f-200ms-run-1/
```

From the paired `base/benchmark` repository, run the local report API against that
parent directory and start the frontend in API mode:

```sh
make build-server
./bin/report-server --local-dir /absolute/path/to/results --port 8080

cd report
yarn install
VITE_DATA_SOURCE=api \
  VITE_API_BASE_URL=http://127.0.0.1:8080/ \
  VITE_ALLOWED_HOSTS=localhost \
  yarn dev --host 127.0.0.1 --port 3000
```

Open `http://127.0.0.1:3000/#/run-comparison/snapshot-throughput`. Filter
`Transaction Payload=blake2f`, choose the desired `Scenario` values and node role,
then set **Show Line Per** to **Block Time Milliseconds**. This renders the 2-second
and 200-millisecond series together while preserving their individual scenarios.
The generated metric files record `BlockNumber` in two-second-equivalent units, so block 1 at
200 milliseconds is `0.1`, block 10 is `1.0`, and a 6,000-block run ends at `600.0`, matching a
600-block two-second run directly on the x-axis.

The report server can synthesize additional comparison rows from the source runs.
Use the original run IDs for raw artifacts; synthetic entries are comparison views,
not additional benchmark executions.

## Cleanup

After the harness exits, retain the result directory and remove or reset only the
disposable datadirs through the environment's snapshot/restore lifecycle.

If a run was interrupted, check for lingering `base-bench` or `base-devnet` processes
before removing its datadirs. Never remove or modify the immutable source snapshot.
