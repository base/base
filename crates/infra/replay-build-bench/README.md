# base-replay-build-bench

Replayed-building benchmark against a private Base mainnet state snapshot.

For each canonical block `i` in the replay range, the harness runs the **real
builder path** and the **real canonical execution path**, in that order:

1. **Build (timed).** Construct `BasePayloadBuilderAttributes` from the
   canonical header of block `i` (timestamp, prev-randao, fee recipient, gas
   limit, Jovian EIP-1559 parameters decoded from the canonical extra data,
   deposit transactions from the canonical block body). Inject the canonical
   block's regular transactions into a real `PendingPool` and drive
   `base_execution_payload_builder::Builder::build` — the same code path the
   live sequencer uses (selection, predicates, ordering, execution, sealing
   including a synchronous state root). The built payload is **discarded**;
   only timings and payload statistics are recorded.
2. **Advance (untimed).** Execute the canonical block `i` with the real block
   executor and fold the resulting bundle state into a durable in-process state
   overlay (`DurableStateProvider`), then into the process-local
   `ExecutionCache`. The overlay is the source of truth for cross-block state;
   the `ExecutionCache` is layered above it as a read-through cache, exactly as
   in production. Every subsequent build and canonical execution therefore
   observes the exact post-state of block `i` regardless of cache capacity or
   eviction order.
3. **Validate.** Receipts and gas usage of the executed canonical block are
   compared against the receipts stored in the snapshot; any divergence aborts
   the run with evidence.

The loop is: build `i` from state at `i-1`, discard the payload, execute
canonical `i` to advance state, then inject `i+1`'s transactions and repeat.

## Scope and known caveats

- The build phase computes the state root **synchronously inside the timed
  interval**; the live sequencer overlaps it with a parallel state-root job.
  Build timings are therefore an upper bound on the sealing component.
- The state root of a *built* payload can diverge from canonical for accounts
  touched by earlier replayed blocks, because trie nodes and hashed state are
  read from the frozen anchor provider (the durable overlay serves
  account/storage/bytecode reads, not trie nodes). This does not affect
  execution semantics, which always observe the correct post-state; validation
  is anchored to the canonical execution receipts, not to built-payload roots.
- No transaction broadcast, no writes to any database: the snapshot is opened
  read-only and all state advancement stays in the process-local durable
  overlay and `ExecutionCache`. Unwinding is discarding the snapshot.
- The durable overlay grows with the replay range (accounts and slots touched
  by replayed blocks); its size is reported as `durable_overlay` in the JSON
  output. `--count` is capped at 1000 blocks.

## Usage

```bash
cargo build --release -p base-replay-build-bench
target/release/base-replay-build-bench --datadir <private-snapshot> --inspect
target/release/base-replay-build-bench --datadir <private-snapshot> \
    --run --count 64 --output results.json
```

The `--prewarm*` flags drive the production prewarm worker pool for an ABBA
prewarm-off vs prewarm-on comparison:

```bash
# A: prewarm off (unchanged build path)
target/release/base-replay-build-bench --datadir <snapshot> --run --count 64
# B: predicate + transaction-simulation warming
target/release/base-replay-build-bench --datadir <snapshot> --run --count 64 \
    --prewarm --prewarm-simulate
```

One `PrewarmWorkerPool` is created before the replay loop (threads spawn once)
and one `PrewarmJob` is started per block just before the timed build, so
workers warm the very `ExecutionCache` the build reads through. Workers open
their own snapshot anchor and layer the shared durable overlay on it, which at
that point holds the canonical post-state of every block *before* the one being
built — the exact parent state, like the builder's
`state_by_block_hash(parent)`. After the timed build the job is joined
(untimed) so no worker can write parent-state reads back into the cache after
canonical advancement. `--prewarm-simulate` requires `--prewarm`, and the JSON
output reports `prewarm.wired` plus `prewarm.active` (a pool with live workers
ran) and `prewarm_totals` (keys and simulations actually scheduled, and
simulations the build loop overtook), so an arm that warmed nothing cannot be
read as a prewarm arm. `PREWARM_WIRING.md` records the wiring plan this
follows.

`--datadir` must be a private snapshot root (private `db/mdbx.dat` plus shared
read-only `rocksdb`/`static_files` links; see `~/perf-tools` on the devbox).
`--from` defaults to `head - count` so the whole replay range is contained in
the snapshot. `--no-build` runs canonical execution + validation only (state
correctness smoke test). Run arms as fresh processes under `perf abba`.
