# One-block deferred state root for Base

This document specifies a bench/devnet-only Base hardfork that moves the execution state commitment one block later. It covers native block construction, validation, persistence, RPC, rollup consumers and comparable measurements. This is a design, not implemented behavior or a performance claim.

## Baseline and evidence

Implement on draft [#6268](https://github.com/base/base/pull/6268), head `432d627174bef54e4b39d7ca9b3e4938300c256e`, reth v2.10.1 `58ebce14ae63dab040beae301e1578cd0dc55e8a`, EVM2 `592bf18c4e22fff54ac9b4db606c4ee276e345cb`. Do not redo that migration or request its already included trie fixes. The design branch is based on main `8ea2a32b7d940902fd819879fee45eaf6c4d7391`; target source is inspected with git show and pinned reth reads, not inferred from the older working files. This target builds native full blocks, not Flashblocks.

Read source maps note_34cb9b753e974620 rev6, note_032e050cbef8b983 rev2, note_d503e7c460d4791c rev3 and overriding reviews note_61ace4082c1586b1, note_e6b6d19d11641664, note_9ee191b06815bbc0. Independent design review note_55020f3e89725f00 rev1 overrides the original design. This revision adopts a narrower no-reth-patch PoC. It removes the claim that inline equality alone prevents a custom job from checking the parent commitment.

Normative references:

- https://eips.ethereum.org/EIPS/eip-7862 , root-only deferral, activation and retained pre-state.
- https://specs.optimism.io/protocol/exec-engine.html and https://github.com/ethereum/execution-apis/blob/main/src/engine/paris.md , payload format and validity/status contracts. Changed root meaning requires an agreed chain fork.
- https://specs.optimism.io/protocol/proposals.html#l2-output-commitment-construction , same-height execution state, message-passer storage root and block hash in OP output v0.

At pinned reth, `StateRootStrategy::{prepare,prepare_payload_builder}` is public in `crates/engine/tree/src/tree/state_root_strategy/mod.rs:104-129`. `StateRootJob::finish` receives the block, finalized execution output and hashed state at `410-427`. `BasicEngineValidator::with_state_root_strategy` is public at `payload_validator.rs:362`. A Base EngineValidatorBuilder can install a custom job. The stock builder does not do this and its invalid-block hook and txpool prewarming setup at `crates/node/builder/src/rpc.rs:1568-1585` must be preserved.

`BlockBuilder::finish_with_state_root`, reth `crates/evm/evm/src/execute.rs:816-841`, finalizes EVM2/BAL and hashes output before the resolver. None means compute a synchronous root, not skip it. Base builder `553-588` already waits inside this resolver. `PayloadStateRootHandle` at `crates/trie/parallel/src/state_root_task.rs:199-302` owns an optional cancellation guard; extracting its receiver does not transfer that guard. Default sharing is opt-in. Do not copy pre-finalization hook closure from older revisions.

The commit-condition API at reth `execute.rs:374-390,783-800` receives borrowed `evm2::TxStateChanges` and TxResult and returns `CommitChanges`. Preserve Base builder `996-1006,1497-1526` admission, metering and predicate callbacks. Compose transaction-level root bookkeeping into those callbacks; retain effects only on Yes and successful commit, never retain the borrowed view. No leaves from rejected speculative transactions may enter root input. Finalized block state remains authoritative for deposits, pre/post system transitions and deletions outside the callback. Test committed, rejected-storage-write and system-only changes together.

## Fork and flag-off contract

Add NEW Base upgrade `DeferredStateRoot` with optional activation timestamp, unset = Never. It is consensus configuration, not a remotely evaluated flag. Only custom bench/devnet schedules may enable it; reject activation on built-in mainnet and Sepolia. No existing upgrade alias or debug skip-state-root. Require activation after genesis and supported OP features; genesis retains today's root meaning.

Let R(k) mean the MPT root after every transition in block k. F is the first non-genesis block with timestamp >= activation T, evaluated independently on each branch.

| Block | Header state_root | Required computation |
|---|---|---|
| F-1 | R(F-1) | Legacy same-block root |
| F | R(F-1) | Bootstrap from verified legacy parent |
| F+1 | R(F) | Independently computed post(F), not header F |
| n >= F | R(n-1) | Branch-specific parent post-state |

Header hash never changes after sealing. BLOCKHASH/parentHash remain final. Two successive headers may carry the same root at activation. Gas, receipts, bloom, transactions, requests, withdrawalsRoot and execution remain same-block. This is not deferred execution or retroactive invalidation.

**Flag-off invariant:** With Never, and for every block before F, identical inputs and deterministic transaction order yield exact header/payload/receipt/state/trie/status/output/proof parity with #6268. Delegate to the stock strategy and stock validator configuration. No manager/root job is allocated before activation. Lazily initialize at F. Preserve invalid-block hook, txpool prewarming, cache behavior and builder trusted insertion when not explicitly running a diagnostic cohort. Mainnet/Sepolia schedules and serialized defaults remain unchanged.

Capture goldens using the immutable #6268 binary and fixture digest before adding deferral, then replay with the candidate off. In-process differential fixtures should also instantiate stock and wrapped validators on independent identical DBs. Assert exact bytes/results and test-only manager constructor/job counters equal zero for Never and pre-F. Test prewarming enabled/disabled and invalid-block hook invocation, not just successful blocks. Counter instrumentation must not add production per-candidate overhead. Diagnostic benchmark settings are explicit cohort inputs, not changes to the off default.

## Chosen no-patch PoC and its limits

The first PoC removes R(n) from the BUILDER seal dependency but deliberately retains R(n) completion before the validator releases an executed block into the tree. It therefore does not promise faster newPayload or earlier canonical acceptance. This choice avoids new fallible persistence machinery and unverified late trie data in reth. A later accept-before-current-root design is separate work, not a hidden assumption here.

Introduce a branch/hash-indexed `PostStateRootManager` in `crates/execution/payload/src/post_state_root.rs` (new). Entry key includes sealed hash, parent hash and number. Pending owns finalized immutable execution output, hashed state, stable parent-state provider/overlay snapshot, cancellation guard and a fallible completion. Ready owns actual R(n), actual TrieUpdates and fully materialized SortedTrieData. Failed is terminal and wakes all waiters. Never key by height alone or treat header.state_root as the actual post-root after activation.

Use a bounded serial provider worker initially. Root(n) starts after builder finalization. Execution(n+1) may overlap it only where the caller has already made that parent available; the native CL pipeline may wait for validation and prevent that overlap. Measure rather than assume overlap. Queue defaults to two uncompleted canonical jobs plus a fixed side-branch allowance. Backpressure admission when full. On abandonment/panic/timeout, resolve the result as failure and stop the branch; do not trust a header or retry forever. Preserve provider snapshots, ordered deltas and pre-state until consumers finish. No DB write transaction is held while waiting.

Parent trie computation uses the DB trie plus actual ordered parent TrieUpdates, not the delayed parent header anchor. The custom EngineValidatorBuilder captures provider and the supplied OverlayManager, so it need not extract private fields from StateRootJobContext. Mirror `payload_validator.rs`'s `overlay_state_provider_factory` construction through public `OverlayStateProviderFactory::new(provider, overlay_manager.overlay_builder(parent_hash))`, using the resolved parent header/block identity. Build jobs use the existing parent provider passed into the builder. Sparse cache sharing is disabled for active serial jobs; stock sparse code compares current roots to current headers at strategy lines 1145/1237 and cannot be reused unchanged.

## Builder and validator flow

Builder targets are `crates/execution/payload/src/builder.rs`, `crates/execution/evm/src/build.rs`, `crates/builder/core/src/service.rs`.

1. Resolve branch-specific R(parent); at F use the verified legacy parent header root. Begin execution with existing EVM2 commit-condition callbacks.
2. Finalize all state/BAL. In finish_with_state_root resolve header root to R(parent), never None in active mode. Temporary empty trie updates may be used ONLY for assembly; they must never enter an executed block or persistence.
3. Seal, bind finalized state to block hash, enqueue root(n), and return payload without awaiting root(n). A parent wait remains possible. Record payload-ready independently from CL acceptance.
4. Initial active mode returns no trusted executed-block object. Local Base CL SealTask -> InsertTask -> newPayload consequently re-executes. This is explicitly included in the benchmark matrix below. Do not silently remove trusted insertion on the ordinary off path.

Implement `DeferredStateRootStrategy` and its job entirely in Base. Off/pre-F delegates to DefaultStateRootStrategy. Active prepare uses its own captured provider/overlay handles and returns a job without installing stock sparse work. Active prepare_payload_builder supplies no shared sparse handle. A Base EngineValidator wrapper delegates all non-root consensus/layout/post-execution checks unchanged.

For a fresh active payload, job.finish performs:

1. Obtain actual R(parent) from the manager keyed by parent hash. An accepted active parent already completed its root; transition uses legacy parent. Missing/disconnected state follows normal engine sync handling, not a header-trusting shortcut.
2. Compare R(parent) to incoming header.state_root. On mismatch, return StateRootJobOutcome with state_root = the COMPUTED R(parent), so the existing inline equality returns BodyStateRootDiff and normal invalid-ancestry/latestValidHash handling. No echo occurs on this path.
3. For a match, compute current R(n) from this validator's finalized output and actual parent overlay, or wait for its correctly associated worker. Do not reuse the builder's root to certify independently executed output without equivalence verification. Initial re-execution cohort independently computes the validator root; retain both roots and assert equality. Imported blocks create their own root input. Complete actual TrieUpdates and SortedTrieData via a bounded fallible result BEFORE returning success.
4. Store ready post-state under n's hash and return `StateRootJobOutcome::new(header.state_root, actual_current_trie_updates)` with the validator's actual hashed state. The echoed scalar is a commitment-check adapter, not R(n). This is safe because its ONLY successful constructor follows the independent R(parent) equality check. It cannot run under debug skip-state-root. Reject startup if that flag is supplied with this fork.

`StateRootJobOutcome` documents its updates as belonging to its root, so this adapter deliberately separates their semantic heights. Stock validator only compares the scalar with the header and uses the actual current updates for the executed state. Audit all uses of the outcome before implementing; metrics/logs naming this scalar a computed current root must be labeled commitment-root in active mode, with actual-current-root emitted separately by Base. No sparse publisher, witness invalid-root path or proof consumer may interpret the echo as R(n). A mismatch uses actual parent commitment data, never a forged same-block diagnostic witness.

The no-patch route relies on `StateRootJob::finish(&block,...)`, not a mathematical loophole in equality. At reth `payload_validator.rs:827` finish executes, `879-897` performs equality. Lines `910-911` release the prewarm cache-save gate, NOT Engine VALID. The successful validator output reaches `tree/mod.rs:3283` insertion before Engine VALID is returned. R(parent), all current execution checks, actual current-root completion and trie materialization all precede that return in this PoC. FCU canonicalizes only accepted blocks, with unchanged safe/finalized ancestry checks. R(n) is not a header assertion in n, but is eagerly computed here for operational simplicity.

Invalid-block diagnostics need an explicit compatibility gate. Stock equality calls on_invalid_block with the returned scalar/updates before rejecting. `crates/engine/invalid-block-hooks/src/witness.rs:242-259` recomputes R(n) and compares it to both that scalar and header(n), assuming same-block roots. Reject the witness invalid-block hook in pre-launch validation whenever the experimental upgrade is configured, even if activation is later. This is an explicit unsupported experimental configuration, not an off-default change: Never chains preserve all stock hook options; supported pre-F runs preserve the configured compatible hook. Do not silently disable an operator-requested hook. Other enabled diagnostic hooks must be audited not to interpret the commitment scalar as actual R(n); reject any incompatible one. A future active-only hook adapter can separate commitment failures from execution diagnostics, but is outside the initial PoC.

Test malformed deferred commitment with a compatible recording invalid-block hook enabled: expect exactly one callback and INVALID, and zero same-block witness generation. Separately assert configured experimental fork plus witness hook fails pre-launch with the exact incompatibility error, while Never plus witness hook preserves stock behavior. On the mismatch-only failure path, empty TrieUpdates may be returned to trigger the stock error; they never enter a successful ValidationOutput or persistence. No witness/proof artifact may label those rejection-only inputs as current-state data.

Wrong deferred root is INVALID with normal latestValidHash parent. Local worker failure is an operational error, not malicious-network INVALID: propagate a non-consensus provider/internal failure, stop advancement and report unhealthy. Do not cache a transient missing-root failure as permanent invalid ancestry. The initial manager is never pending for a previously accepted active parent. If future code permits pending parent roots, add an explicit bounded retry owner in Base `crates/consensus/engine/src/task_queue/tasks/insert/task.rs` before using SYNCING: current InsertTask accepts Syncing then proceeds to FCU and does NOT retry the same hash automatically. Pending-root SYNCING is not a supported liveness mechanism in this PoC.

## Trie publication, insertion and persistence

The correct target type is `LazyTrieData`, `crates/trie/common/src/trie_data.rs:49-135`. `crates/chain-state/src/deferred_trie.rs` does not exist at the target. `ExecutedBlock::with_deferred_trie_data`, `crates/chain-state/src/in_memory.rs:825`, can carry late data, but this PoC does not rely on unbounded late-root publication.

`LazyTrieData::pending`/get can wait forever if its producer is dropped. Never put a root-pending handle into the engine tree. Root computation and sorting use a Base-owned Result channel with deadline, panic/cancel/drop propagation; only success constructs LazyTrieData::ready / ExecutedBlock::new. No dummy empty data on failure.

Even the inner BasicEngineValidator normally spawns a pending sorting producer in `payload_validator.rs:1524-1560`. The Base wrapper must replace its successful `ValidationOutput`'s executed block with one built from the job's ready SortedTrieData and identical recovered block/output/BAL/timing data, without calling get on the inner pending handle. The manager retained these inputs in finish. Drain or cancel the redundant inner sorting task with normal runtime ownership; count its work in benchmarks. Before exposing to tree/persistence, assert ready publication; a producer-drop fault must return a bounded operational failure, not enter persistence. If the public output constructors do not permit this replacement, that is a demonstrated implementation blocker: use a narrowly scoped fallible-publication reth change before benchmarking, not an indefinite wait. Do not claim no-patch completion until the wrapper integration test passes.

For future optimized trusted insertion, `EngineValidator::on_inserted_executed_block` (`payload_validator.rs:1853`) is wrappable and can check parent commitment and require ready actual trie data before delegating. Initial active mode disables it, and benchmark controls mirror that choice. AlreadySeen(Valid) is safe only for a hash previously checked or equivalently checked trusted insertion. Test the local fast path explicitly; later newPayload will not repair an unchecked insertion.

Existing persistence receives only ready actual data, not header-derived or fake trie updates. MDBX transactions are NOT atomic with static files and RocksDB. Do not add a fictitious cross-store checkpoint table or promise existing crash rollback is fork-aware. Actual post-roots are manager-owned for the run; restart rules below deliberately do not depend on inferring them from a delayed header.

## Reorg and restart scope

Memory reorgs retain state/deltas and roots per hash. Switch parent providers and manager entries together; late results cannot replace another branch's root. Test activation-crossing memory reorg and competing same-height forks. A deep reorg requiring staged backfill/unwind is unsupported initially and must stop the experimental node before those stages run. Never disable Merkle unwind alone and keep advancing with an inconsistent trie.

At reth `crates/node/builder/src/launch/common.rs:574-660`, ordinary startup checks MDBX/static-file/RocksDB consistency and partial-trie/pipeline-unwind markers, then may run unwind stages. Merkle unwind still compares against same-height header at `crates/stages/stages/src/stages/merkle.rs:427`; forward Merkle checks at 362. This can happen after a routine crash, not just corruption. Node::disabled_stages alone cannot safely fix it; partial-trie recovery explicitly re-enables Merkle unwind.

Initial restart policy is disposable benchmark state, not general restart recovery. Base CLI/run preflight BEFORE reth launch requires an immutable consistent snapshot whose tip is strictly pre-F, or a fresh genesis DB. Persist a run-start marker outside binary-reset DB directories before first active operation. Any existing active-run marker rejects reopening that datadir, including clean shutdown, before reth startup recovery. Operator restores a new copy of the known pre-F snapshot and replays from there through Engine API. Never auto-delete a DB or marker; benchmark harness selects a fresh numbered directory. A crash does not reopen the active DB. Unmarked user-supplied DBs must be inspected and rejected if any store/checkpoint indicates active history or consistency/unwind uncertainty; accepted snapshots have recorded hashes and provenance. No claim of recovering an active durable tip or ordinary crash restart is made.

Gate staged full/backfill/snap sync and automatic fallback for active experiments at node configuration/admission. A request beyond engine replay capacity stops the run. Later production work must make startup multi-store recovery and Merkle execute/unwind fork-aware, add authenticated post-root checkpoints and test interrupted writes at each store boundary. That later work may require reth changes; it is not necessary to demonstrate this disposable no-patch execution experiment.

## RPC, derivation, outputs and proofs

Engine JSON shape, required stateRoot, method versions, payload IDs and block-hash verification remain unchanged; meaning changes only under the shared chain schedule. Ordinary receipt fields, logs and calls/balances/storage at n remain post(n). eth_getBlock(n).stateRoot becomes R(n-1). A finalized block hash is never patched later.

eth_getProof(n) still means post(n), never n-1. It authenticates against actual R(n), which a light client locates in child n+1.stateRoot. An experimental actual-root endpoint must return state hash/height, actual root and committing child hash/height, distinguishing locally computed from child-authenticated data. Pin tag requests to hashes before waiting; enforce canonical ancestry. Initial PoC fails closed for active proof RPC unless this adapter is implemented and verified. Pending synthetic roots are not canonical authentication. Historical consumers that assume header(n).stateRoot authenticates post(n) are not compatible without an adapter.

Base CL uses the same fork schedule in rollup config and payload derivation. Batches still derive attributes, not roots, so need no new field. Test unsafe import, safe re-execution and FCU end to end before claiming derivation support. External op-node needs schedule/output support; unchanged op-node is not asserted compatible. Initial isolated EL/CL execution benchmark can exclude L1 derivation, but must label that exclusion.

Preserve `output(n) = keccak256(bytes32(0) || R(n) || W(n) || hash(n))`. W(n) is post(n) message-passer storage root, still same-block withdrawalsRoot after Isthmus. Never mix R(n-1) with W(n)/hash(n). `crates/consensus/engine/src/query.rs:103` must use a Base actual-post-root adapter, not header.state_root; wait for branch-correct child authentication before publishing outputs at the required safety level. Initial PoC disables output publication and production proof/withdrawal proving/finalization services, rather than returning an incorrect tuple.

Later proof support must update `crates/proof/executor/src/builder/assemble.rs`, revm-backed proof execution, both single/batch comparisons in `crates/execution/trie/src/live.rs`, its provider/batch_provider, witnesses, challenges, zk outputs and proposer. Returning actual TrieUpdates is not enough to fix those same-height header assumptions. Published v0 encoding alone does not establish compatibility with deployed dispute/validity verifiers or L1 contracts. Their required program/verifier upgrades need separate scope and review. Never connect the experimental configuration to real bridge contracts. Same-block withdrawalsRoot computation/validation stays enabled in all measured arms.

## Ordered implementation plan

1. Start on #6268 head432d627. Retain its official v2.10.1/EVM2 pins; run just check-reth-pin and just pin-reth-test. `etc/upstream-pins/reth.toml` is authoritative. No fork repin is required for the hook PoC. If a demonstrated publication/recovery blocker requires a patch, document it first, update manifest/carried patches, generate with just pin-reth and recheck; do not hand-edit generated Cargo pins.
2. Write tests below first against immutable #6268; save exact commands, fixture hashes and real initial failures in numbered logs. Parity already passes and must stay passing. This design-only task claims no executable red result.
3. Add schedule/feature in `crates/common/genesis/src/{chain/upgrade,feature,rollup,schedule}.rs`, `crates/common/chains/src/upgrades.rs`, `crates/execution/chainspec/src/{upgrades,spec,builder}.rs`, `crates/execution/cli/src/chainspec.rs`. Add pre-launch disposable-state guard in `crates/execution/cli`/`crates/execution/runner/src/node.rs` before reth launch. Reject built-in-network activation and unsupported services/sync modes.
4. Add manager and strategy/job types in new `crates/execution/payload/src/post_state_root.rs` and re-export from minimal lib.rs. Capture provider/OverlayManager in Base EngineValidatorBuilder, installed via `crates/execution/node/src/{node,rpc,engine}.rs` and runner wiring. Mirror stock invalid-block hook/prewarm configuration. Add active-only validator output wrapper to materialize ready trie data before tree insertion. Verify public constructors support every preserved field before proceeding.
5. Change `crates/execution/payload/src/{builder,validator,metrics}.rs`, `crates/execution/evm/src/build.rs` and `crates/builder/core/src/service.rs`. Active assembly chooses parent root; expose no trusted executed object initially. Keep current-root provider jobs real, branch-correct and fallible. Preserve commit-condition callbacks and all system changes. Never allocate a manager on off/pre-F branches.
6. Add benchmark-only matched insertion/root-algorithm controls, explicit in custom-chain config, not new defaults. Instrument independent execution/root counts, cache hits and duplicate sorting. Match root services and restrictions in both controlled arms.
7. Implement or fail closed for proof RPC/actual-root adapters; update output query only for an explicitly enabled extended phase. Add rollup schedule and service restrictions in `crates/consensus/engine`/RPC. Do not claim proof or L1 support prematurely.
8. Add chained tests in `crates/builder/core/tests/forks.rs` and systems runtime `etc/systems/src/l2/runtime.rs`, plus colocated manager/validator tests. Use relevant just test recipes; user handles fmt/clippy/zepter. No new crate unless dependency placement demands it.
9. Hand immutable candidate/baseline/config/fixture digests and correctness logs to base-metal. No deploy or base-bench execution is authorized by this design.

## Test-first acceptance

| Must-hold test | Exact observable requirement | Expected #6268 result |
|---|---|---|
| flag_off_parity | Golden and differential exact headers/payloads/receipts/state/trie/status/output/proofs, zero manager/job allocations, stock hook/prewarm behavior | Pass and stay passing |
| activation_parent_post_state | Distinct R(F-1),R(F),R(F+1); header F=R(F-1), F+1=independent R(F) | F same-block root assertion fails |
| validate_post_parent | Header n=R(n-1), normal execution checks and FCU succeed; actual R(n)/updates ready before insertion | Legacy equality rejects delayed payload |
| reject_wrong_deferred_root | Rehash header with R(n) instead of distinct R(n-1); INVALID and exact latestValidHash parent; descendants/FCU cannot bypass | Legacy accepts same-block commitment, rejection assertion fails |
| seal_not_accept_does_not_wait_current_root | Barrier blocks R(n), releases parent; getPayload seals, but validator does not release executed data until actual R(n) and ready trie data | Legacy builder waits; no early acceptance claim |
| echo_requires_parent_check | Tampered parent commitment cannot reach success/echo constructor; checked parent root returned on mismatch | New hook contract absent |
| no_pending_persistence | Drop/panic root and sorting producers, timeout bounded; no successful ValidationOutput/tree/persistence admission; no LazyTrieData.get wait | New safety contract absent |

Also require commit Yes/No/system-only mixed root tests; gas/receipts/withdrawalsRoot rejection; malformed block hash; revert/delete/recreate/deposit/empty blocks; resource limits and low-CPU behavior; cancellation and queue pressure; duplicate payload hashes; on_inserted_executed_block bypass attempt; two-branch/activation reorg; late result isolation; unsupported deep reorg/sync refusal; marker rejected before startup recovery on clean or crashed active DB; new pre-F snapshot replay exactly reproduces roots. Assert both ordinary off default and benchmark controls independently. Proof adapters, output tuple and derivation tests are gates for their extended phases. Every assertion has exact expected values and descriptive messages. First failures must be actual assertion failures, not missing-build infrastructure. Record numbered logs without overwrite; predicted failures here are not evidence.

## Fair benchmark matrix and accounting

Ordinary #6268 returns Some(executed) at builder599-613, then InsertExecutedBlock (`tree/mod.rs:1605`); own newPayload may be AlreadySeen at3191. Active initial mode instead re-executes via SealTask/InsertTask -> newPayload. Comparing those two alone measures an implementation tradeoff with asymmetric extra execution, not a causal effect of root deferral.

Required cohorts, all on the same v2.10.1/EVM2 source and deterministic fixture/config except listed controls:

| Cohort | Commitment and root algorithm | Insertion/execution work |
|---|---|---|
| O ordinary reference | Same-block, ordinary default root settings | Build once; trusted executed insertion; normal cached own newPayload |
| S serial trusted control | Same-block serial provider, sharing off | Build once, root once; trusted insertion; isolates algorithm from ordinary streaming options |
| R serial re-execution control | Same-block serial provider, sharing off | Disable trusted insertion explicitly; builder execution/root plus validator re-execution/root |
| D deferred initial PoC | Parent commitment, serial provider, sharing off | Build execution + detached root; no trusted insertion; validator re-execution + independent current root, parent commitment check, manager and ready-publication work |

Primary D vs R holds root algorithm and re-execution policy fixed. R vs S measures insertion/re-execution penalty. S vs O measures root-algorithm change where ordinary configuration differs. D vs O is the complete experimental implementation versus ordinary baseline and must be labeled as confounded by both insertion and root algorithm; never call it isolated deferral speedup. Optional sparse streaming reference is a separately labeled cohort with exact flags/CPU prerequisites, not the default.

List/count all incremental work per block: builder and validator tx execution and hashing, each current-root calculation, parent lookup/check, manager queue/coalescing/snapshots, cancellation, root sorting and inner validator redundant sorting, execution cache hits/misses, post-execution checks, actual-vs-builder root comparison, trusted insertion and own-newPayload status. D deliberately computes two independent roots on sequencer re-execution, as R does; coalescing must not hide validation or change execution counts between arms. Record any later optimization as a new cohort. Apply the same ready-output wrapper/sorting controls in R and D to measure incremental manager/commitment cost, and report residual duplicate sorting if not equalized. Independent validator nodes always execute/root each imported block in all cohorts, with counts reported separately from sequencer counts.

Metrics include seal latency ms (finalized execution to immutable payload ready, separate parent wait), building ms (job start to ready and FCU->getPayload), validation ms (newPayload arrival->response, execution/current-root/check/sort portions), and end-to-end accepted validator gas/s, included successful tx/s and submission-to-inclusion latency. Add re-execution count, tx count, gas and wall/CPU ms per block, trusted-insertion count and AlreadySeen count. VALID timing is measured at Engine response, not cache-save notification. A faster getPayload with unchanged/slower CL acceptance is not an end-to-end gain.

Report p50/p90/p95/p99 with sample counts and repeated paired trials; root enqueue/start/finish/wait CPU and wall time, execution/root overlap, queue lag/size, persistence lag, DB I/O, RSS, context switches, failures/cancels/fallbacks, missed cadence. Existing native_payload_state_root_duration is residual finishing work, not pure root CPU. Use monotonic timestamps correlated by hash and fixed structured messages with selected fields; avoid per-candidate logging-only allocations.

Base-metal must expose immutable Base/reth/EVM2/image digests, cohort and insertion/root controls, genesis/fork schedule/chain ID, matching EL/CL configuration, all service restrictions, queue/deadline/worker limits, same snapshot digest and fresh numbered datadir policy, state size, workload/seed/order, gas limits/cadence/fees, warmup/windows, caches, persistence/pruning, CPU affinity/quota/memory. Root-worker dedicated cores are a labeled experiment, not an invisible D advantage. Keep ordinary matrix and publication keys unchanged; isolate candidate outputs and do not overwrite prior data. Match disabled proof/output/sync services across controlled R/D runs. Separate simulated execution, sequencer acceptance and independently observed validator throughput.

Use storage/account-heavy traces, deletes/reverts and rates through saturation, not just transfers. Drain every measured root and persistence task before final accounting; growing backlog invalidates gains. Include wall time through drain, not just publication window. Functional root checks precede performance claims. Actual base-bench runs, deployments and publication require separate coordinator/user authorization.

## Remaining scope and decisions

No semantic question blocks this isolated hook PoC. It may show no sequencer throughput gain because acceptance still waits for R(n) and local re-execution. That is an honest result, not grounds to skip validation. Access to registered base-metal and future benchmark authorization remain necessary.

Accept-before-current-root, optimized checked trusted insertion, robust active-database crash recovery, staged/snap sync, external op-node, proof/withdrawal/output publication and possible L1 verifier/contract upgrades require separate design and tests. In particular, a future pending-root persistence path must provide fallible completion before admission; LazyTrieData::pending is not that protocol. Mainnet/Sepolia behavior stays unchanged.
