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

## Full execution and batching

Capture an **entire ordered block** to execute real transactions, internal calls,
precompiles, system calls, fees, receipts and storage writes without a node:

```sh
target/release/base-replay capture --block safe --limit 100000 --execution --compact \
  --out execution.json.zst
target/release/base-replay verify execution.json.zst
target/release/base-replay bench execution.json.zst --samples 15 --iterations 3
```

`--execution` checks the transaction root before requesting state. A partial
selection is rejected, because its parent-state witness is not the prestate of an
arbitrary later transaction. The production `StatelessL2Builder` drives capture:
account/storage hints request parent-hash `eth_getProof`, code reads request
`eth_getCode`, and BLOCKHASH requests capture ancestor headers. Only a witness
that reconstructs the **entire canonical mainnet header** is exported. Subsequent
`verify` and `bench` commands use only that file, including the real EVM and trie
updates. `--compact` omits the duplicated RPC tape, not the execution witness.
HTTP 429/5xx responses retry five times with bounded exponential backoff.

Deletion can require a sibling node omitted from ordinary EIP-1186 proofs.
Capture first tries a child-hash proof: a collapsed leaf/extension has a longer
path, so its old encoding can be recovered **only if its hash matches the missing
parent commitment**. It never substitutes child values for parent values. If
necessary it probes an absent key sharing the sibling's prefix, requesting a
real parent proof. This fallback is bounded to six nibbles and 2^26 attempts;
deeper/unavailable witnesses fail explicitly. Public RPC capture can take minutes
and depends on retained state. It needs no debug API or trace-derived zero state.

`fixtures/mainnet-execution.json.zst` captures block 52,541,820: all 137
transactions, 41,070,754 gas, 14,063 trie nodes and 257 bytecodes. It was captured
with `--block 52541820 --limit 100000 --execution --compact`; compression is
lossless Zstandard JSON. `capture --out NAME.zst`, `verify`, and `bench` support
this format, and decompression is outside benchmark timing. This block contains
no BLOCKHASH ancestor reads; header-oracle performance is not represented by it.
`fixtures/historical-isthmus.json` adds all 185 transactions of block 32,000,000,
captured with `--block 32000000 --limit 100000 --compact` without historical state.

The checked-in `fixtures/batch/b52540000.json` through `b52540011.json` contain
12 consecutive full mainnet blocks: 1,815 transactions (1,803 non-deposits),
including legacy, EIP-1559 and EIP-7702 envelopes. Generate equivalent inputs with
`capture --block NUMBER --limit 100000 --compact`; proofs are unnecessary here.

```sh
target/release/base-replay bench-batch crates/infra/replay/fixtures/batch/*.json \
  --samples 15 --iterations 2 > batching.json
```

This runs production batch composition, Brotli compression, framing, blob packing,
blob decoding, channel reassembly, decompression and strict batch decoding. It
checks decoded transactions, ordering, timestamps, parents and epochs against the
source fixtures before timing. The default corpus spans multiple blobs/frames.
The library also accepts calldata and custom `EncoderConfig` settings. Frame IDs
are random; decoded batches and sizes, not byte-for-byte channel IDs, are stable.
Selections without transaction 0 use an explicitly counted synthetic zero-epoch
L1-info deposit for **component benchmarks only**; include index 0 to preserve
the real epoch. These regenerated batches are not claims about what L1 posted.

## Component boundaries

| Substitute | Library API | Production code exercised |
| --- | --- | --- |
| CL/EL transaction input | `Fixture::payload()` → `BasePayloadAttributes` | Canonical envelope decoding and signature recovery used by block/proof execution |
| Transaction fee input | `Fixture::transactions` → encoded envelopes | `base-common-flz::data_gas_fjord`, shared by sequencing and proof execution |
| State storage | `Fixture::provider()` → `WitnessProvider: TrieProvider` | `base-proof-mpt::TrieNode::open` and `blind` |
| RPC | `RecordedRpc: Rpc` | Exact method/parameter response replay; capture itself can run against this substitute |
| Proof oracle | `ExecutionWitness: TrieDBProvider + TrieHinter` (by reference) | Real bytecode, ancestor headers and trie nodes with no network fallback |
| EL execution | `EvmWorkload` | `BaseBlockExecutorFactory`, precompiles, fees, system calls, receipts and state transitions |
| CL/EL block input | `Fixture::execution_payload()` | `StatelessL2Builder`, fork-dependent fee parameters and canonical block commitments |
| Batcher/L1 DA | `BatchWorkload::encode()` | `BatchEncoder`, `FrameEncoder`, `BlobEncoder` |
| Derivation data | `BatchWorkload::decode()` | `BlobDecoder`, `Frame`, `Channel`, `BatchReader` |

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
- `evm-execute`: a cold trie database, system calls, all EVM transactions, receipts
  and merged state transitions. Decoding/signature recovery are prepared outside
  timing; trie read/bytecode access costs are included.
- `receipts-root`: fork-aware production receipt commitment calculation.
- `storage-update-root`: apply the actual mainnet block's account/storage changes
  and reconstruct its state root, including trie cloning to reset each iteration.
- `stateless-block`: the entire stateless builder, including input preparation,
  signature recovery, execution, state updates, commitments and header sealing.
- `bench-batch` / `batch-encode`: composition through DA packing, including block
  cloning into the owning encoder.
- `bench-batch` / `batch-derive`: DA decoding through strict batch decoding.

These are **native component** benchmarks, not end-to-end sequencer throughput or
ZK proof generation/cycle counts. Storage is an in-memory proof-backed substitute,
not MDBX disk I/O. No running CL/EL service, mempool ordering, P2P transport,
Engine API transport, L1 receipt verification, epoch/sequencing-window validation,
reorg handling, KZG commitment generation, L1 submission or prover orchestration
is timed. Batch derivation supplies all frames at one synthetic inclusion block;
it does not exercise channel timeouts. Extend the fixture boundary for those
processes rather than attributing these timings to them.
