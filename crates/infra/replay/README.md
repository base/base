# base-replay

Capture selected **Base mainnet** transactions once, then benchmark real production
components without a running node, RPC endpoint, persistent database, or prover service.

```sh
cargo build --release -p base-replay-bin
target/release/base-replay capture --block safe --indices 1,3,5 --proofs \
  --storage 0x4200000000000000000000000000000000000015:0x1 --out fixture.json
target/release/base-replay verify fixture.json
target/release/base-replay bench fixture.json > before.json
# Build a candidate change, then use the SAME fixture and machine:
target/release/base-replay bench fixture.json > after.json
```

For a completely offline start, a small recorded mainnet fixture is checked in:

```sh
target/release/base-replay bench crates/infra/replay/fixtures/mainnet.json
cargo test -p base-replay -p base-proof-mpt -p base-common-flz --lib
```

That fixture contains indices 0, 1 and 3 from block 52,539,765, six account proofs,
and `L1Block` storage slot 1 at its parent. It was generated with `capture --block
52539765 --indices 0,1,3 --proofs --storage
0x4200000000000000000000000000000000000015:0x1`; only `rpc.exchanges` was cleared
to avoid checking in the entire source block's RPC response. It is suitable for
component tests, not replaying capture's RPC calls.

`--block` accepts decimal, hex, or an RPC block tag. Capture resolves it once and
pins all state requests to the parent **hash** using EIP-1898. Recent safe blocks
are recommended: public RPCs often retain only a short proof window. An RPC that
cannot supply the requested proofs fails capture; it never silently drops state.
Use `--rpc-url` or `BASE_REPLAY_RPC_URL` for your own archive endpoint. URLs and
credentials are not saved. Capture uses read-only RPC methods.

Select by comma-separated `--indices`, `--transactions` hashes, `--to` address,
and `--limit` (default 32). Filters intersect, then the limit applies; original
block order is preserved. Indices/hashes that do not exist are errors. Capture
multiple blocks with separate invocations, keeping one fixture per source block.
Omit `--proofs` to use a historical endpoint without state/proof support.
With proofs, senders and recipients are captured automatically; `--storage` adds
explicit account/slot accesses. Internal call accounts are **not** inferred.

## Component boundaries

| Substitute | Library API | Production code exercised |
| --- | --- | --- |
| CL/EL transaction input | `Fixture::payload()` → `BasePayloadAttributes` | Canonical envelope decoding and signature recovery used by block/proof execution |
| Transaction fee input | `Fixture::transactions` → encoded envelopes | `base-common-flz::data_gas_fjord`, shared by sequencing and proof execution |
| State storage | `Fixture::provider()` → `WitnessProvider: TrieProvider` | `base-proof-mpt::TrieNode::open` and `blind` |
| RPC | `RecordedRpc: Rpc` | Exact method/parameter response replay; capture itself can run against this substitute |

`Capture::capture` accepts any `Rpc`; `OpenedState::read` accepts any
`TrieProvider`. Tests can replace those boundaries independently. Unknown RPC
requests and missing trie nodes fail; there is no online or zero-state fallback.
The JSON response tape can also feed a separate RPC server adapter; this tool does
not start a mock HTTP server or replace the running CL/EL services.

Validation checks schema/chain identity, header linkage, transaction hashes and
recovered senders, and EIP-1186 values against reconstructed state/storage roots.
Headers are RPC-sourced, not consensus-verified. Fixtures are trusted workload
inputs, not an authentication protocol for untrusted peers.

## What the numbers mean

`bench` emits raw batch-normalized samples, median, p10 and p90 in nanoseconds per
**whole selected workload**, plus fixture/block hashes and workload counts. Use
release builds, a quiet machine, identical inputs, and multiple independent runs.
Increase `--iterations` for very short workloads; `--samples` defaults to 30.
Loading JSON, network capture, witness indexing, and initial correctness checks
are outside timing. Component allocations and output destruction are included.

- `payload-decode`: canonical decoding.
- `payload-recover`: decoding plus sender recovery and encoded-envelope assembly.
- `fjord-data-gas`: Fjord compression/pricing for non-deposit transactions. This
  is the named algorithm, **not** historical total fees or fork selection.
- `proof-open-cold`: fresh blinded tries, reads, decoding and value checks.
- `proof-read-warm`: the same accesses against already opened tries.
- `proof-root`: rehash opened account/storage paths, retaining blinded siblings.

These are native component benchmarks, **not** end-to-end sequencer throughput,
full transaction execution, or ZK proof generation/cycle counts. Parent-block
proofs are not the prestate of arbitrary later transactions. Selecting a subset
does not preserve the source block's state root. Full EVM replay requires exact
pre-transaction state (including internal calls, bytecode and block hashes), or
the entire ordered block with an execution witness. Full proof replay also needs
L1 derivation inputs and the proof stack's existing witness oracle. Public RPCs
may disable the necessary debug methods; this tool deliberately does not fake
that coverage.
