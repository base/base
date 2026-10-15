---
name: adding-replay-fixtures
description: Adds real Base mainnet examples to base-replay for offline testing and benchmarking. Use for a slow block, a specific transaction, or a new batching, derivation, execution, or storage workload.
---

# Adding replay fixtures

Capture real mainnet data at a fixed block, verify it offline, and retain the smallest fixture that represents the requested workload.

Run commands from the repository root. Read `crates/infra/replay/README.md` for the fixture format, component boundaries, and capture limitations. Check `target/release/base-replay capture --help` against these instructions before using an existing binary; rebuild if it is stale:

```sh
cargo build --locked --release -p base-replay-bin
```

## Choose the fixture boundary

| Question | Capture | What it supports |
| --- | --- | --- |
| Where does block-building CPU time go? | Entire block with `--execution`, then builder `mainnet_build` bench | Native full-block selection/execution, state-key hashing and payload assembly with flat mock state; no sequencer roots or disk I/O |
| Why is this block slow in proof execution or proof state updates? | Entire block with `--execution` | Shared EVM with proof `TrieDB`, receipts, proof trie updates/root, and full proof stateless builder |
| Is this transaction expensive to decode, recover, or price? | `--transactions HASH` or `--indices INDEX` | Selected-envelope component benchmarks |
| Which account/storage reads are costly? | Selection plus `--proofs` and explicit `--storage ADDRESS:SLOT` | Parent-state proof reads and roots, not transaction execution |
| Is batching or derivation expensive for this traffic? | Consecutive blocks, all transactions, no proofs needed | Composition, compression, frames/blobs, channel reassembly, batch decoding |

**Separate proof execution from critical-path sequencing.** `base-proof-mpt::TrieNode` and `StatelessL2Builder` belong to the proof pipeline used by the TEE prover. Label trie and witness-backed execution results as `proof-*`. The builder's `mainnet_build` benchmark instead prepares flat reads outside timing and removes the proof database before running the native full-block builder in `base-execution-payload-builder`. It mocks `state_root_with_updates` and excludes disk I/O. A slow sequencer block is a useful input, but its proof replay time does not explain its sequencer latency. Host measurements are not TEE measurements; enclave overhead, attestation and signing are excluded.

**A selected transaction is not independently executable against parent-block state.** Earlier transactions may change its prestate. To investigate its EVM behavior, capture its entire containing block and preserve ordering. The current tool reports whole-block execution, not per-transaction EVM timings; do not claim to isolate the target's execution cost or remove earlier transactions from the witness.

## A slow block

1. Start with the reported block number. If asked to find candidates, inspect a bounded, explicitly chosen range of recent safe blocks. Gas used, transaction count, and calldata size are candidate signals, not proof of slow execution. Rank candidates using repeated offline measurements, including an ordinary-block control.
2. Use a concrete block number in retained examples. Tags such as `safe` are resolved once by capture; record the resulting number and hash. Capture pins state to the parent hash. Recent blocks are likelier to be within a public RPC's proof-retention window.
3. Set `BLOCK` to the chosen decimal number, then capture:

```sh
FIXTURE="crates/infra/replay/fixtures/b${BLOCK}-execution.json.zst"
target/release/base-replay capture --block "$BLOCK" --limit 100000 \
  --execution --compact --out "$FIXTURE"
target/release/base-replay verify "$FIXTURE"
target/release/base-replay bench "$FIXTURE" --samples 15 --iterations 3 \
  > /tmp/replay-block-bench.json
```

Require `full mainnet header matched` from verification. Capture exports only after the production builder reconstructs the canonical header and a second, offline execution agrees. Partial selection, missing state, wrong code, and unavailable proofs must fail, not be replaced with zero state.

An old block needs an endpoint retaining its parent proofs and code. Use the existing `BASE_REPLAY_RPC_URL` environment variable for a supplied endpoint; do not print credentials, save them in fixtures, or include them in commands/logs committed to Git. If state is unavailable, report the limitation or use transaction-only capture explicitly. Do not silently substitute another block for a specifically requested one. Capture may take minutes and can fail when required sibling nodes cannot be recovered through public proof APIs.

## Sequencer block building

Use the same complete execution fixture with the real builder benchmark, after running offline verification:

```sh
BASE_REPLAY_FIXTURE="$FIXTURE" CARGO_PROFILE_BENCH_DEBUG=0 \
  cargo bench --locked -p base-replay --bench mainnet_build -- --test
BASE_REPLAY_FIXTURE="$FIXTURE" CARGO_PROFILE_BENCH_DEBUG=0 \
  cargo bench --locked -p base-replay --bench mainnet_build -- \
  --warm-up-time 1 --measurement-time 2
```

Compilation requires libclang; `.agents/setup` installs it. The test-mode run checks the canonical header except its deliberately uncomputed state root, all transaction bytes/order, gas, and a missing-read negative control. The withdrawals root is explicitly mocked. The production FIFO pool reproduces block order, not historical competing transactions or arrival times. Fixtures must have leading deposits followed by pool transactions. Report `build-mocked-roots`, `select-execute-flat-state`, `pool-select-commit`, `transition-merge` and `state-key-hashing` separately; stages overlap and must not be added. These are full-block workloads, with no Flashblocks publication or progressive assembly. Do not claim end-to-end block latency, root computation, metering-service enforcement or network fanout. The separate builder `state_root` benchmark uses real RocksDB with **synthetic** state, not mainnet state.

Retain Criterion raw samples from `target/criterion` for before/after comparisons. Use identical fixtures and benchmark parameters in at least three alternating runs of preserved baseline/candidate binaries; record binary hashes. Do not run capture or compilation concurrently with timing.

## A specific transaction

Set `TX_HASH` to the requested hash. Resolve its containing block with a read-only `eth_getTransactionByHash` call or a trustworthy explorer; reject pending/not-found results. Set `BLOCK` to that mined block number. The CLI's hash selector searches only the supplied block; it does not find the block globally.

```sh
FIXTURE="crates/infra/replay/fixtures/b${BLOCK}-tx-${TX_HASH}.json"
target/release/base-replay capture --block "$BLOCK" --transactions "$TX_HASH" \
  --compact --out "$FIXTURE"
target/release/base-replay verify "$FIXTURE"
target/release/base-replay bench "$FIXTURE" --samples 15 --iterations 100 \
  > /tmp/replay-transaction-bench.json
```

Check that the fixture retains the requested hash and original transaction index. For known account/storage accesses, add `--proofs` and repeat `--storage ADDRESS:SLOT`; slots are hexadecimal. This captures selected senders/recipients and explicitly requested slots, not every internal call's state. Filters (`--indices`, `--transactions`, `--to`) intersect; use only those needed. The default selection limit is 32.

Use a descriptive suffix instead of the full transaction hash when useful, but retain the exact hash in provenance. Omit `--compact` only when the example needs `RecordedRpc` response replay. Compact fixtures deliberately omit that tape; they still retain selected transactions and any proofs/execution witness.

## Batching and derivation

Capture a bounded consecutive range with `--limit 100000 --compact`, one file per block, without `--execution` or `--proofs`. Keep related files in a named subdirectory under `crates/infra/replay/fixtures/`. Pass paths in numeric block order, not an unchecked glob that might sort different-width numbers incorrectly:

```sh
target/release/base-replay bench-batch "$FIRST_FIXTURE" "$SECOND_FIXTURE" \
  --samples 15 --iterations 2 > /tmp/replay-batch-bench.json
```

The command checks round-trip transaction bytes, ordering, timestamps, parents, and epochs before timing. Include transaction index 0 to retain real L1-info. A subset omitting it uses a counted synthetic zero-epoch deposit and is only a component example. Regenerated batches are not the exact historical L1 submissions. Inspect frame count when claiming a multi-frame/blob workload.

## Validate and keep the example

- Reuse an existing fixture if it already covers the requested behavior. Keep a new one only when its workload is distinct; explain why. Prefer compressed `.json.zst` for large execution witnesses. Capture refuses to overwrite existing paths.
- Verify the saved file, not just the in-memory capture, and run the appropriate benchmark offline. `verify` and both benchmark commands have no network fallback. Fixture loading, decompression, and validation are outside stage timing.
- Record block number/hash, selected indices/hashes, capture flags, fork/transaction types, relevant workload counts, and the reason for retaining it in `crates/infra/replay/README.md`. State which boundaries it covers and what it cannot demonstrate. Never claim gas used alone proves a performance bottleneck.
- For a performance claim, compare the same fixture on release builds before and after, on the same machine with identical sample/iteration settings. Use separate Cargo target directories, verify binary hashes, alternate runs, and report medians across at least three runs plus raw samples. Include an unchanged control. Keep raw timing exports outside the fixture corpus; in an orb, reviewable evidence belongs under `.amp/in/artifacts/`.
- Always set sample/iteration counts explicitly: the full-block `bench` defaults (30 samples, 100 iterations) are much slower than the quick 15-by-3 command above. A transaction-only workload needs more iterations because each operation is short.
- Run `cargo test --locked -p base-replay --lib`. Add a permanent regression assertion only for a concrete observable contract the new example exercises; extend existing tests before adding another test file. No wall-time thresholds in unit tests.
- Follow the `update-docs-index` skill for Markdown changes, then run `git diff --check`. Inspect fixture size and provenance before committing. Do not commit scratch logs, endpoint credentials, or duplicated RPC tapes without a reason.
- Report separately: capture cost, offline benchmark wall time, measured component timings, and coverage gaps. Native proof execution is not TEE prover latency, ZK proving/cycle measurement, disk MDBX performance, or critical-path sequencer block building. Builder flat-state timings cover critical-path CPU components, not storage/transport or end-to-end latency. Do not infer TEE or `no_std` proof-runtime gains from host measurements.
