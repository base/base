# Snapshot Benchmarking

This guide covers repeatable performance measurements on an L1-free Base mainnet
snapshot devnet. It explains how to run `base-bench snapshot`, publish its output
to `base/benchmark`, and select the exact runs to compare.

The snapshot benchmark is for local performance investigation. It mutates its
builder and validator datadirs, so every attempt needs a fresh writable clone pair.

## Prerequisites

- Build an optimized `base-bench` binary. Debug builds are not suitable for a
  400M-gas performance measurement.
- Start from an immutable, fully prepared Base mainnet datadir snapshot.
- Use separate writable builder and validator clones of that exact snapshot.
- Keep the source snapshot read-only for benchmark purposes; only destroy the
  disposable per-run clones.

On a ZFS host, create a pair for one run without copying the underlying chain data:

```sh
SOURCE_DATASET=zroot/data/snapshots/base-mainnet@benchmark-source
RUN=blake2f-2s-run-1
BUILDER_DATASET=zroot/data/snapshots/${RUN}-builder
CLIENT_DATASET=zroot/data/snapshots/${RUN}-client

sudo zfs clone "$SOURCE_DATASET" "$BUILDER_DATASET"
sudo zfs clone "$SOURCE_DATASET" "$CLIENT_DATASET"

BUILDER_DATADIR=/home/meyer9/snapshots/${RUN}-builder
CLIENT_DATADIR=/home/meyer9/snapshots/${RUN}-client
```

Do not reuse either clone for another attempt, including a retry. A benchmark
advances the chain and changes cache and database state.

## Run One Benchmark

Build and run the snapshot harness from this repository:

```sh
cargo build --release -p base-system-tests --bin base-bench

export BASE_BENCH_CLIENT_VERSION="base/$(git rev-parse --short HEAD)"

target/release/base-bench snapshot \
  --builder-datadir "$BUILDER_DATADIR" \
  --client-datadir "$CLIENT_DATADIR" \
  --load-test-config crates/infra/load-tests/examples/blake2f-mainnet-snapshot-2s.yaml \
  --benchmark-run snapshot-throughput \
  --scenario blake2f-2s-run-1 \
  --run-id blake2f-2s-<timestamp> \
  --output-dir results/blake2f-2s-run-1
```

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

## Choose Comparable Runs

Use the fields below consistently. They have different purposes.

| Field | Purpose | Comparison rule |
|---|---|---|
| `--benchmark-run` | Report cohort | Use the same value for every candidate in one comparison, such as `snapshot-throughput`. |
| `--scenario` | User-visible experiment/repetition label | Use a descriptive value for each selected run, such as `blake2f-2s-run-1` or `blake2f-200ms-run-1`. |
| `--run-id` | Immutable artifact identifier | Keep it unique. It does not group runs. |
| `--output-dir` | Complete local artifact directory | Keep it unique. Never allow two runs to write to the same directory. |
| `BASE_BENCH_CLIENT_VERSION` | Binary/build label | Keep it equal for cadence comparisons; deliberately vary it only for client-version comparisons. |

For a fair 2-second versus 200-millisecond Blake2f comparison:

1. Clone both roles from the same immutable snapshot for every repetition.
2. Use the same release binary, host, load parameters, seed, and funding policy.
3. Run `blake2f-mainnet-snapshot-2s.yaml` with 30 measured blocks.
4. Run `blake2f-mainnet-snapshot-200ms.yaml` with 300 measured blocks.
5. Give both cases the same `--benchmark-run`; use distinct scenarios and artifact IDs.
6. Alternate cadence order across repetitions (`2s`, `200ms`, then `200ms`, `2s`) to reduce thermal and cache-order bias.

Both supplied Blake2f configurations cover the same 60-second window. The 2-second
case uses 30 blocks; the 200-millisecond case uses 300 blocks.

Keep maximum-throughput tuning separate from fixed-offered-load comparisons. Record
whether runs use a warm or cold page-cache policy; ZFS clone equivalence alone does
not make page-cache state equal.

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

The report server can synthesize additional comparison rows from the source runs.
Use the original run IDs for raw artifacts; synthetic entries are comparison views,
not additional benchmark executions.

## Cleanup

After the harness exits, retain the result directory and destroy only the disposable
datasets:

```sh
sudo zfs destroy "$BUILDER_DATASET"
sudo zfs destroy "$CLIENT_DATASET"
```

If a run was interrupted, check for lingering `base-bench` or `base-devnet` processes
before destroying its clones. Never destroy the immutable source snapshot.
