# Action Tests

Action tests are a framework for integration-testing the Base rollup protocol
by composing simulated actors and driving them through discrete, reproducible
scenarios. The pattern draws on prior end-to-end harness designs, but is
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
    ├── harness.rs              ActionTestHarness
    ├── matrix.rs               ForkMatrix (upgrade combinations)
    ├── test_rollup_config.rs   TestRollupConfigBuilder
    ├── engine.rs               ActionEngineClient
    ├── l1/
    │   ├── miner.rs            L1Miner, L1 blocks, signed txs, receipts, reorgs
    │   ├── provider.rs         SharedL1Chain, ActionL1ChainProvider
    │   ├── block_fetcher.rs    ActionL1BlockFetcher
    │   └── blob.rs             ActionBlobProvider
    ├── l2.rs                   L2Sequencer, ActionL2Source, TestAccount
    ├── p2p.rs                  SupervisedP2P, TestGossipTransport
    ├── node.rs                 TestRollupNode, derivation / verifier pipelines
    ├── batcher/
    │   ├── actor.rs            Batcher actor
    │   └── tx_manager.rs       L1MinerTxManager (inbox submission)
    └── providers/
        └── l2.rs               ActionL2ChainProvider
    tests/                      integration tests - one scenario per module (subdirs when grouped)
```

All actors live in the single `base-action-harness` crate. Action tests are
integration tests in `harness/tests/`; related scenarios may be grouped under a
shared integration-test target with one module per scenario. Unit tests for
individual actor internals live as inline `#[cfg(test)]` blocks inside the
actor's source file.


## How actors work

The simplest actors implement the `Action` trait:

```rust
pub trait Action {
    type Output;
    type Error: core::fmt::Debug;
    fn act(&mut self) -> Result<Self::Output, Self::Error>;
}
```

`act()` performs one discrete step and returns a typed result. Tests can call
`act()` directly for actors that implement the trait, or call more descriptive
methods such as `L1Miner::mine_block()` and `Batcher::advance()`. The trait
exists so a test harness can drive a heterogeneous list of simple actors
uniformly when that is useful.

Actors are plain Rust structs. They own their state, and tests drive their
public methods directly. Simple actors mutate each other through explicit
references. Production-shaped actors use channels or background tasks when
that is part of the behavior under test; for example, `Batcher` owns a
background `BatchDriver` task and exposes methods that let tests stage,
mine, confirm, fail, or reorg L1 submissions at precise points.


## L1Miner

`L1Miner` maintains an in-memory chain of `L1Block`s. Each block holds a
consensus `Header`, ordered signed `TxEnvelope` bodies, matching consensus and
RPC-shaped receipts, and optional EIP-4844 blob sidecars. This is the shape
the production derivation pipeline reads from L1.

When the batcher wants to submit data to L1, `L1MinerTxManager` signs the
candidate as an EIP-1559 or EIP-4844 transaction and stages it for the miner.
The miner drains staged transactions into the next block when `mine_block()`
is called. Tests can also enqueue synthetic L1 events such as deposits,
system-config updates, gas updates, and operator-fee updates; those helpers
wrap each log in a signed transaction and attach the log to that
transaction's receipt.

The block header uses `alloy_consensus::Header` and calls `hash_slow()` to
compute parent hashes, so the in-memory chain has a realistic hash structure
that the derivation pipeline can traverse.

Safe and finalized head pointers are explicit test state. Tests advance or set
them with `act_l1_safe_next`, `act_l1_finalize_next`, `act_l1_safe`, and
`act_l1_finalize`, which keeps finality and reorg scenarios deterministic.
Tests that need more control can read `block_by_number()` directly.


## Signed L1 data and the derivation pipeline

On a real network, batcher transactions are EIP-1559 transactions where:

- `to` is the batch inbox address from the rollup config
- `from` is the known batcher address
- `input` starts with `DERIVATION_VERSION_0` (0x00) followed by encoded
  channel frames

The derivation pipeline's L1 retrieval stage filters L1 transactions by
comparing `to` against the batch inbox address and `from` against the expected
batcher address. It then extracts `input` as raw frame data.

The harness now models that boundary with signed L1 transactions instead of a
loose transaction shortcut. `BatcherConfig` carries a deterministic L1 signer,
and `L1MinerTxManager` uses it to build signed calldata or blob submissions.
`ActionL1ChainProvider` returns those signed transaction bodies to
`EthereumDataSource`, while `ActionBlobProvider` serves blob sidecars by
versioned hash. The derivation pipeline therefore exercises the production
calldata and blob DA sources even though the underlying L1 chain is still
in-memory.


## ActionL2Source and BaseBlock

The batcher actor needs to read L2 blocks in order to know what to batch.
`ActionL2Source` is a `VecDeque<BaseBlock>` that implements
`L2BlockProvider`. Tests usually fill it with blocks produced by
`L2Sequencer`, which uses the production L1 origin selector, attributes
builder, and in-process engine client. Each block therefore contains a real
L1-info deposit transaction and signed user transactions, rather than a
batcher-only mock shape.

`ActionTestHarness::create_l2_source(n)` is the shortcut for building a source
with `n` sequenced blocks. Tests that need precise block contents can create
an empty `ActionL2Source`, build blocks through `L2Sequencer`, and push them
manually.


## Batcher actor

`Batcher` drains `BaseBlock`s from an `L2BlockProvider` and forwards them to a
production `BatchDriver` running in a background tokio task. The driver owns a
`BatchEncoder`, channel manager behavior, calldata/blob frame construction,
and submission flow. The harness-owned boundary is `L1MinerTxManager`, which
turns the driver's transaction candidates into signed L1 transactions and
lets tests control when those transactions are staged, mined, confirmed,
failed, or reorged.

For the common happy path, call `batcher.advance(&mut h.l1).await`: it drains
the L2 source, flushes the encoder, mines one L1 block, and confirms the
resulting receipts. For more exact scenarios, use `encode_only`,
`stage_n_frames`, `confirm_staged`, `fail_next_n_submissions`, `reorg`, and
`wait_until_requeued`.


## Writing a test

```rust
use base_action_harness::{ActionTestHarness, Batcher, BatcherConfig};

#[tokio::test]
async fn example_action_test() {
    let mut h = ActionTestHarness::default();
    let batcher_cfg = BatcherConfig::default();

    // Step 1: mine some L1 context.
    h.mine_l1_blocks(3);
    assert_eq!(h.l1.latest_number(), 3);

    // Step 2: build real L2 blocks and batch them into one L1 block.
    let source = h.create_l2_source(5).await;
    let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg);
    batcher.advance(&mut h.l1).await;

    // Step 3: inspect the signed L1 submissions.
    assert!(h.l1.latest_number() >= 4);
    assert!(
        !h.l1.tip().transactions.is_empty() || !h.l1.tip().blob_sidecars.is_empty(),
        "mined block should contain signed batcher submissions"
    );
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
