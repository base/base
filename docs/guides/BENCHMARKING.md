# Benchmarking

This guide explains which Base benchmark to run, what each one needs, and where results go. All
benchmarks are driven by `base-bench` (`etc/systems`, crate `base-system-tests`), which starts a
development network, runs the load generator from
[`base-load-tests`](../../crates/infra/load-tests/README.md) against it, and writes results. For the
networks themselves, see [Development Networks](DEVNETS.md).

Always use an optimized build (`--release`) for performance numbers. Debug builds are functional
smoke tests only; debug payload execution is CPU-bound far below the configured block gas limit.

## Which benchmark do I want?

| I want to... | Command | Network | Needs |
|---|---|---|---|
| Smoke-test plain transfers | `just devnet bench` | Fresh devnet, L1 in Docker | Docker |
| Measure a set of workloads on empty state | `base-bench local --workload-config ...` | One fresh devnet per workload, L1 in Docker | Docker |
| Measure against real mainnet or Sepolia state | `base-bench snapshot ...` | Snapshot devnet, no L1 | Two writable restores of a Reth datadir |
| Combine several snapshot runs into one report | `base-bench aggregate ...` | None | Existing run output directories |

The two `base-bench` network modes trade realism for cost:

- **Fresh devnet (`local`)** starts from empty state, so every workload is reproducible and isolated
  but does not reflect the size of mainnet state. Each workload gets a newly provisioned devnet so
  token state, accounts, the transaction pool, and caches cannot leak between scenarios.
- **Snapshot devnet (`snapshot`)** continues a real Base datadir, so trie depth, database size, and
  cache behavior resemble production. It mutates its datadirs, so every attempt needs a fresh
  writable restore.

## Quick local transfer benchmark

With no arguments, `base-bench` starts a fresh temporary devnet, runs the default 60-second
plain-transfer profile, prints the load-test summary, and shuts everything down:

```bash
just devnet bench
```

It needs Docker, but no snapshot and no funded key. The generated datadirs are temporary and are
removed during shutdown. `just devnet bench <args>` forwards its arguments to
`cargo run --release -p base-system-tests --bin base-bench`.

## Fresh-devnet workload suite

The checked-in suite in [`etc/benchmarks/fresh-devnet.yml`](../../etc/benchmarks/fresh-devnet.yml)
runs each workload on its own empty devnet. It currently covers B-20 transfers (with the B-20 asset
feature activated before setup), high-concurrency ETH transfers to new and existing recipients, and a
50,000-round Blake2f precompile profile:

```bash
cargo run --release -p base-system-tests --bin base-bench -- local \
  --workload-config etc/benchmarks/fresh-devnet.yml \
  --output-dir results/fresh-devnet \
  --client-version "base/$(git rev-parse --short HEAD)"
```

The command writes one native load-test sidecar per workload plus a top-level visualizer manifest:

```text
results/fresh-devnet/
├── metadata.json
├── suite-results.json
├── fresh-devnet-b20-transfer/load-test-result.json
├── fresh-devnet-eth-new/load-test-result.json
├── fresh-devnet-eth-existing/load-test-result.json
└── fresh-devnet-blake2f-50000/load-test-result.json
```

`metadata.json` and the load-test sidecars are directly consumable by the static visualizer in
`base/benchmark`; link this output directory to that repository's ignored `output/` directory and run
its normal production build.

Swap workloads can opt into contract provisioning with `deploy_devnet_swap_harness: true` on a
workload entry. That deploys a fresh devnet USDC token plus Uniswap/Aerodrome router shims for each
workload, then auto-wires swap and real-token setup addresses before execution.

### In CI

The opt-in Depot workflow (`.depot/workflows/bench-fresh-devnet.yml`) runs this suite for trusted
`base/base` pull requests with the `bench:tps` label. It publishes raw and visualizer artifacts and
updates one advisory PR comment with the workload summaries.

## Snapshot benchmarks

`base-bench snapshot` runs one load test against a Base snapshot continuation and writes a
self-contained `base/benchmark` run directory. It owns the process lifecycle around the load test but
never creates, copies, or deletes the caller-provided datadirs.

Snapshot benchmarking has its own guide because it is the most involved workflow: disposable
restores, comparable-run selection, 2s versus 200ms fairness, report artifact layout, and cleanup.
See [Snapshot Benchmarking](SNAPSHOT_BENCHMARKS.md). The `snapshot` network is described in
[`etc/systems/README.md`](../../etc/systems/README.md#snapshot-devnet).

## Viewing results

Both modes write results that `base/benchmark` can display: `local` writes a visualizer bundle,
and `snapshot` writes per-run directories that the report server compares. The snapshot guide's
[Compare In Base/benchmark](SNAPSHOT_BENCHMARKS.md#compare-in-basebenchmark) section shows how to run
the report server against a results directory.

## See the exact options

```bash
cargo run --release -p base-system-tests --bin base-bench -- local --help
cargo run --release -p base-system-tests --bin base-bench -- snapshot --help
cargo run --release -p base-system-tests --bin base-bench -- aggregate --help
```
