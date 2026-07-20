# base-action-harness

`base-action-harness` is the in-process integration-test harness for rollup
action tests. It lets tests drive L1, batcher, sequencer, verifier, engine,
P2P, and finality actors one step at a time while keeping the scenario
deterministic and cheap to debug.

This harness is intentionally not a devnet. It does not start real services,
open RPC sockets, or depend on Docker. Its job is to run the production Rust
protocol components wherever that is practical, and to make every synthetic
boundary explicit where the action framework still owns test-only behavior.

## What Runs Today

The current harness is more than a pure mock. Several core paths already use
production components:

- The sequencer builds `BasePayloadAttributes` with the production L1 origin
  selector and stateful attributes builder.
- The engine client executes blocks through the production `BasePayloadBuilder`
  against a temporary Reth database.
- The verifier drives the real derivation pipeline and applies derived payloads
  through the in-process engine client.
- The batcher actor runs the production `BatchDriver` and `BatchEncoder`.
- Each verifier node opens a real `SafeDB` in a temporary directory.

The main synthetic pieces are the L1 chain, transaction manager, blob sidecar
store, P2P transport, conductor behavior, and finality/reset orchestration.

## Production Boundary Map

| Component | Production Code Exercised | Harness-Owned Boundary | Main Production Gap |
| --- | --- | --- | --- |
| L1 chain and miner | Alloy `Header`, block hash chaining, signed `TxEnvelope` bodies, consensus receipts consumed by derivation, RPC-shaped transaction receipts and log metadata for batcher confirmations and L1 events | `L1Miner`, `L1Block`, manual reorg/safe/finalized heads | No tx pool, contract execution, full gas accounting, or beacon sidecar service |
| L1 calldata DA | Verifier nodes use `EthereumDataSource` and production `CalldataSource` over signed tx bodies | In-memory `ActionL1ChainProvider` backed by `SharedL1Chain` | RPC paging/provider edge cases are not covered by the default action path |
| L1 blob DA | Verifier nodes use `EthereumDataSource`, production `BlobSource`, versioned hashes from signed EIP-4844 txs, and `ActionBlobProvider` sidecar lookup | Blob sidecars are stored in `L1Block::blob_sidecars` rather than fetched from a beacon API | Beacon API behavior, blob retention windows, and sidecar transport are not modeled |
| Batcher | `BatchDriver`, `BatchEncoder`, channel manager behavior, span/single batch encoding, signed calldata/blob tx construction, txpool-blocked → `cancel_tx` recovery | `L1MinerTxManager`, in-memory L2/L1 event channels, synthetic inclusion receipts | Submission does not use a real RPC tx manager, mempool, replacement, or fee bumping (nonce-slot blockage is modeled; recovery is exercised via `cancel_tx`) |
| Sequencer | L1 origin selection, attributes building, payload construction, real signed L2 user txs | Test actor lifecycle and manual stepping | No real node service loop, txpool/RPC ingress, engine transport, or production unsafe block scheduling |
| Engine | `BasePayloadBuilder`, Base EVM config, temporary Reth database, enforced state-root verification (asserts against the sequencer's root; `assert_state_roots_verified` proves it ran) | `ActionEngineClient` implements only the Engine API behavior tests need | Simplified payload statuses, forkchoice handling, transaction pool, networking, persistence lifecycle, and Engine API edge cases |
| Verifier and derivation | Real derivation pipeline, attributes queue, reset signals, payload application, `SafeDB` | `TestRollupNode` orchestration and manual L1 push/signals | Reset/finality/unsafe-head flow is test-scripted rather than driven by production driver loops and online providers |
| P2P and unsafe gossip | Optional production unsafe-block signing formula | `SupervisedP2P` and `TestGossipTransport` are in-memory | No libp2p peer scoring, mesh behavior, networking, throttling, or gossip timing |
| Conductor | Exercises high-level sequencing/follower roles | In-memory conductor control surface | No production service integration, RPC control plane, or multi-process failure modes |

## What Tests Are Good At Today

Action tests are currently strongest for deterministic protocol-level
scenarios where the important behavior lives inside the Rust components:

- Batcher channel construction, frame ordering, gap filling, requeueing, and
  upgrade behavior.
- Sequencer/verifier agreement on derived payloads and state roots.
- Derivation behavior across upgrade transitions, origin changes, drift,
  deposits, system-config updates, and L1 reorgs.
- `SafeDB` persistence behavior tied to derived safe heads.
- Unsafe gossip acceptance and rejection when in-memory signing validation is
  enabled.

They are weaker for behavior that depends on production IO boundaries: L1 RPC
provider behavior, tx-manager replacement, beacon blob retrieval, service
lifecycle, and real network timing.

## Known Production Gaps

The following gaps are intentional today, but should stay visible when adding
new tests:

- The default verifier path now uses production calldata/blob DA sources, but
  the L1 provider is still an in-memory provider rather than an RPC provider.
- Blob DA computes versioned hashes from signed EIP-4844 transactions and
  fetches matching blobs through `ActionBlobProvider`, but the sidecars still
  live in memory instead of behind a beacon API.
- Receipts are synthetic. They preserve block hash, block number, timestamp,
  transaction index, log index, sender, recipient, gas fields, bloom filters,
  and blob gas markers for signed transactions, but they do not come from L1
  contract execution.
- Derivation logs can still be enqueued directly on `L1Miner`. The harness now
  wraps each enqueued log in a signed synthetic L1 event transaction and attaches
  the log to that transaction's receipt. This is useful for focused derivation
  tests, but it still does not prove the emitting contract path.
- Finality, safe-head movement, resets, and reorgs are explicit test actions.
  They are not yet driven through the same online driver and consensus-client
  signals production receives.
- P2P and conductor tests exercise local state transitions but not real network
  or service integration.
- L1 contract execution is not modeled. The harness does not run `SystemConfig`,
  `OptimismPortal`, or other L1 contracts; event helpers encode their expected
  logs directly, and `ActionL1BlockFetcher::get_logs` is intentionally narrow.
  Behavior that depends on real RPC/contract semantics (e.g. `eth_getLogs`
  filtering, deposit/system-config event shape, beacon-sidecar compatibility)
  belongs in an opt-in external-L1 test backed by a real local L1, not the
  default in-process path.
- Full service lifecycle, RPC servers, config/CLI bootstrap, P2P mesh behavior,
  EL/CL coupling, and multi-process failures are out of scope for action tests
  by design; they are covered by the Docker-backed system tests.

## Working With the Harness

The production-shaped synthetic L1/DA path is the default:

1. `BatcherConfig::default()` includes a deterministic L1 signer, and
   `with_l1_signer` updates the batcher address to match.
2. Use `create_test_rollup_node` or `create_test_rollup_node_from_sequencer`
   for verifier nodes; both exercise `EthereumDataSource`.
3. Build direct L1 test transactions with `L1TxBuilder` or
   `L1Miner::submit_calldata_transaction` so calldata tests still exercise
   signer recovery and inbox filtering.
4. Use the L1 event helpers for system-config, operator-fee, and deposit tests
   so derivation reads logs from signed transaction receipts.
5. Use `Batcher::stage_n_frames`, `Batcher::confirm_staged`, and
   `Batcher::staged_count` when a test needs to distinguish submission from L1
   inclusion.
6. Use `Batcher::fail_next_n_submissions` and `Batcher::block_next_n_submissions`
   (paired with `Batcher::cancellation_count`) to drive the production
   `BatchDriver` failure and txpool-blocked recovery paths.
7. Call `TestRollupNode::assert_state_roots_verified` when a test should prove
   the engine actually compared derived state roots against the sequencer's.
