# Action Tests

Action tests are a framework for integration-testing the Base rollup protocol
by composing simulated actors and driving them through discrete, reproducible
scenarios. The pattern is borrowed from Optimism's op-e2e Go framework, but
implemented in Rust and built directly on the same types the production node
uses.

The central idea is that every participant in the rollup — L1 block producer,
batcher, sequencer, verifier — is modelled as a lightweight actor that exposes
a small state machine interface. A test assembles whichever actors it needs,
calls their methods in a scripted sequence (the "actions"), and then asserts on
the resulting chain state. There are no real nodes, no network sockets, no
Docker containers, and no async runtimes required unless an actor genuinely
needs them.


## Why action tests?

Unit tests for isolated functions are fast but can miss emergent bugs at
protocol boundaries. End-to-end devnet tests are thorough but slow, fragile,
and hard to debug. Action tests sit in the middle: they run in milliseconds,
they exercise real protocol logic (the same batch encoding, channel
compression, and derivation pipeline that runs in production), and they fail
with a clear stack trace pointing at the exact step that broke.

Concretely, an action test can verify things like:

- A batch submitted by the batcher in L1 block N is picked up by the
  derivation pipeline and advances the safe head on L2.
- Submitting frames across multiple L1 blocks still produces a valid channel
  when reassembled.
- A sequencer that skips an epoch produces batches the verifier correctly
  rejects.


## Architecture

```
actions/
└── harness/        base-action-harness crate
    src/
    ├── lib.rs                  public API (re-exports)
    ├── action.rs               Action trait, L2BlockProvider trait
    ├── miner.rs                L1Miner, L1 blocks, PendingTx, reorgs
    ├── l2.rs                   L2Sequencer, ActionL2Source, TestAccount
    ├── harness.rs              ActionTestHarness
    ├── matrix.rs               ForkMatrix (upgrade combinations)
    ├── test_rollup_config.rs   TestRollupConfigBuilder
    ├── p2p.rs                  SupervisedP2P, TestGossipTransport
    ├── engine.rs               ActionEngineClient
    ├── node.rs                 TestRollupNode, derivation / verifier pipelines
    ├── batcher/
    │   ├── actor.rs            Batcher actor
    │   └── tx_manager.rs       L1MinerTxManager (inbox submission)
    └── providers/              L1 / L2 / blob sources for pipelines
        ├── l1.rs               SharedL1Chain, ActionL1ChainProvider, ActionDataSource
        ├── l1_block_fetcher.rs ActionL1BlockFetcher
        ├── l2.rs               ActionL2ChainProvider
        └── blob.rs             ActionBlobDataSource, blob DA
    tests/                      integration tests - one scenario per module (subdirs when grouped)
```

All actors live in the single `base-action-harness` crate. Action tests are
integration tests in `harness/tests/`, one file per scenario. Unit tests for
individual actor internals live as inline `#[cfg(test)]` blocks inside the
actor's source file.


## How actors work

Every actor implements the `Action` trait:

```rust
pub trait Action {
    type Output;
    type Error: core::fmt::Debug;
    fn act(&mut self) -> Result<Self::Output, Self::Error>;
}
```

`act()` performs one discrete step and returns a typed result. Tests can call
`act()` directly, or call the actor's more descriptive methods (e.g.
`L1Miner::mine_block()`). The trait exists so a test harness can drive a
heterogeneous list of actors uniformly when that is useful.

Actors are plain Rust structs. They own their state and do not communicate
through shared memory, channels, or async runtimes. If one actor needs to
write to another — for example the batcher needs to submit a transaction to
the L1 miner — it takes a mutable reference to the target actor. The borrow
checker enforces that only one actor mutates state at a time, which eliminates
a whole class of test flakiness that plagues Go's goroutine-based approach.


## L1Miner

`L1Miner` maintains an in-memory chain of `L1Block`s. Each block holds a
consensus `Header` (number, timestamp, parent hash, base fee) and a list of
`PendingTx` entries representing the batcher transactions included in that
block.

When the batcher wants to submit a batch to L1, it calls
`L1Miner::submit_tx(PendingTx { from, to, input })`. The miner accumulates
pending transactions and drains them into the next block when `mine_block()`
is called. This mirrors what happens on a real L1: the batcher broadcasts a
signed transaction, it enters the mempool, and the next block proposer
includes it.

The block header uses `alloy_consensus::Header` and calls `hash_slow()` to
compute parent hashes, so the in-memory chain has a realistic hash structure
that the derivation pipeline can traverse.

Safe and finalized head pointers lag behind the latest head by 32 and 64
blocks respectively, approximating Ethereum's post-merge consensus behaviour.
Tests that need more control can read `block_by_number()` directly.


## PendingTx and the derivation pipeline

On a real network, batcher transactions are EIP-1559 transactions where:

- `to` is the batch inbox address from the rollup config
- `from` is the known batcher address
- `input` starts with `DERIVATION_VERSION_0` (0x00) followed by encoded
  channel frames

The derivation pipeline's L1 retrieval stage filters L1 transactions by
comparing `to` against the batch inbox address and `from` against the expected
batcher address. It then extracts `input` as raw frame data.

`PendingTx` in the harness captures exactly those three fields. No
cryptographic signing is required for action tests because the derivation
pipeline does not verify signatures — it only reads the sender address and
calldata. When we later wire up a derivation actor in action tests, the
`L1Block::batcher_txs` field provides the same interface that a real provider
would give the pipeline.


## MockL2Source and MockL2Block

The batcher actor needs to read L2 blocks in order to know what to batch.
`MockL2Source` is a `VecDeque<MockL2Block>` that the batcher drains. Tests
pre-populate the source either by calling `generate()` (which creates
sequential blocks with incrementing numbers and timestamps) or by constructing
`MockL2Block` values manually when they need specific field values.

`MockL2Block` carries only the fields the batcher inspects: block number,
parent hash, timestamp, the L1 origin (epoch number and hash), and raw encoded
transactions. It does not wrap a full `SealedBlock` or `BasePayloadAttributes`
because the batcher does not need the rest of the block structure.


## Batcher actor (coming next)

The batcher will drain `MockL2Block`s from the `L2BlockProvider`, construct a
`SingleBatch` per block using `base_protocol::SingleBatch`, feed those batches
into a `base_comp::ChannelOut<BrotliCompressor>` at `BrotliLevel::Brotli10`
(the compression level Base uses in production), and call
`L1Miner::submit_tx()` for each output frame.

The frame encoding follows the OP Stack derivation spec:

```
[DERIVATION_VERSION_0] ++ channel_id (16 B) ++ frame_number (2 B)
    ++ frame_data_length (4 B) ++ frame_data ++ is_last (1 B)
```

Using `base_comp::ChannelOut` directly (rather than a wrapper) means the
action tests exercise the same code path as the real batcher — if there is a
bug in frame encoding or compression, an action test will catch it.


## Writing a test

```rust
use base_action_harness::{Action, ActionTestHarness, MockL2Block, PendingTx};
use alloy_primitives::{Address, Bytes};

#[test]
fn example_action_test() {
    let mut h = ActionTestHarness::default();

    // Step 1: mine some L1 blocks.
    h.mine_l1_blocks(3);
    assert_eq!(h.l1.latest_number(), 3);

    // Step 2: generate mock L2 blocks and submit a batch.
    h.generate_l2_blocks(5);
    // (batcher.advance() will go here once the batcher actor is implemented)

    // Step 3: mine another L1 block to include the batcher tx.
    h.l1.mine_block();
    assert_eq!(h.l1.latest().batcher_txs.len(), 1);
}
```


## Usage

Add to `Cargo.toml`:

```toml
[dev-dependencies]
base-action-harness.workspace = true
```

Run the action tests:

```
just actions test
```

Or run them directly with cargo:

```
cargo nextest run -p base-action-harness
```
