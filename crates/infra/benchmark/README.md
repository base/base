# base-benchmark

Benchmark orchestrator for Base EL clients (BaseRethNode, Builder). Drives consensus via Engine API, collects Prometheus metrics, and invokes `base-load-test` as the transaction payload worker.

## Usage

```bash
cargo build -p base-benchmark
./target/release/base-bench --config examples/devnet.yaml --root-dir /tmp/bench --output-dir /tmp/bench/output
```

See [`PLAN.md`](PLAN.md) for full implementation details and [`PORTING_REFERENCE.md`](PORTING_REFERENCE.md) for the Go-to-Rust porting reference.
