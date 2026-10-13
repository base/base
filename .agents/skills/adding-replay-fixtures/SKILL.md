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
| Why is this block slow to execute or update state? | Entire block with `--execution` | Real EVM, receipts, storage updates/root, and full stateless builder |
| Is this transaction expensive to decode, recover, or price? | `--transactions HASH` or `--indices INDEX` | Selected-envelope component benchmarks |
| Which account/storage reads are costly? | Selection plus `--proofs` and explicit `--storage ADDRESS:SLOT` | Parent-state proof reads and roots, not transaction execution |
| Is batching or derivation expensive for this traffic? | Consecutive blocks, all transactions, no proofs needed | Composition, compression, frames/blobs, channel reassembly, batch decoding |

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
- Report separately: capture cost, offline benchmark wall time, measured component timings, and coverage gaps. Native stateless execution is not ZK proving/cycle measurement, disk MDBX performance, or end-to-end sequencer throughput. Do not infer `no_std` proof-runtime gains from native measurements.
