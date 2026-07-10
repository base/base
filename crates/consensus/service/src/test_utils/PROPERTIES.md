# CL/EL invariant properties tested by this harness

This document is the authoritative catalogue of every property (safety, liveness,
choreography) that the Tier-0 test harness in this directory checks. Every test
in `test_utils/*.rs`, plus the anchor tests in `crates/consensus/engine/src/`,
maps to one or more properties here.

Read this file before adding tests, deleting tests, or moving state-machine code
in `engine/` or `service/actors/`. If you change what one of these properties
means, update it here first.

**Groundwork — the state model:**

- `EngineState = { sync_state: EngineSyncState, el_sync_finished: bool, need_fcu_call_backup_unsafe_reorg: bool }`
- `EngineSyncState = { unsafe_head, local_safe_head, safe_head, finalized_head }` — four `L2BlockInfo`s (block number + hash + parent + L1 origin + sequence-number).
- `EngineSyncStateUpdate` — same four fields, each `Option<L2BlockInfo>`. This is the composite that PR #3803 mishandled.

**Labelling.** `S` = safety, `D` = derivation, `Sq` = sequencer-only, `V` = validator-only, `C` = cross-role, `L` = liveness, `F` = flashblocks external. Where a property `X` is provably a consequence of others we write `X [⇐ ...]` and keep `X` as first-class only when it's easier to test directly than to test its components.

---

## Anchor bugs — the tests we must never regress

Two production bugs anchor Tier 0. If the test suite ever passes on the pre-fix commit for either, the suite is broken and must be repaired before shipping any change.

### Anchor #1 — PR #3803 / backport #3809 (`2f014dae6`, merged 2026-07-07)

`fix(consensus-engine): preserve safe heads on syncing FCU`. `SynchronizeTask::execute` built a composite `new_sync_state` from a composite `EngineSyncStateUpdate` (advancing safe *and* unsafe), sent forkchoice to the EL, got `Syncing`, and dropped **everything** — including the already-consolidated safe advance. Derivation waited forever on a `NewAttributesConfirmed` that never came. Pure liveness bug. Fix separated `updated()` (pure computation) from `apply_update()` (metric-committing commit), and on `Syncing` commits only the fields at-or-behind current unsafe with `el_sync_finished` sticky-true.

**Bug class (generalised):** *"When part of a composite update is rejected by a downstream component, does the CL correctly commit the sub-part that was not rejected?"* Expected siblings live in `insert/`, `consolidate/`, `finalize/`.

**Tests:** `task_queue::tasks::synchronize::task_test::tests::anchor_3803::anchor_3803_composite_update_matrix` (16 rstest cases). Also proved by proptest at `test_utils::proptest_model::tests::injected_3809_regression_trace_survives_syncing_composite_update`. Also surfaces as a two-node liveness-gate failure in `test_utils::two_node::c1_syncing_response_wedges_validator_liveness_gate_catches`.

### Anchor #2 — PR #2698 (`4836ea189`, merged 2026-05-27)

`fix(consensus): Recover Pruned Restart Checkpoints`. On startup, the CL walked its labeled safe/finalized heads via `find_starting_forkchoice`. If the EL had pruned the bodies of those blocks, block hydration failed with `MissingL1InfoDeposit` and the node wedged at sync start. Fix added a mandatory CL-side `ForkchoiceCheckpointReader` seam populated by the new checkpoint actor. When EL pruning is detected, the CL falls back to its own validated checkpoint after cross-checking against the EL-labelled block header.

**Tests:** `sync::tests::anchor_2698_recover_pruned_safe_finalized_with_checkpoint_reader` (2 rstest cases: `case_1_noop_reader_errs`, `case_2_scripted_reader_recovers`).

### Anchor #3 (hardening) — PR #2967 (`30d5d98d2`)

`fix(consensus/engine): make pruned-tip binary-search recovery robust`. Two corner cases in `find_earliest_unpruned_block`: (a) `latest_number == pruned_block_number` (loop never executes), (b) `latest` itself pruned (search converges on the very block that raised the error).

**Tests:** `sync::forkchoice::tests::anchor_2967_binary_search_corner_cases` (parametric over `(latest, pruned_boundary)`).

---

## S — State-level safety (both roles)

Every `S` property must hold at every reachable state.

- **S1. Head ordering & ancestry.** `finalized.number ≤ local_safe.number ≤ safe.number ≤ unsafe.number`, and each head is a canonical ancestor of the next.
- **S2. Head monotonicity.**
  - **S2.a Finalized:** strictly monotonic — never regresses under any protocol event.
  - **S2.b Safe / local-safe:** non-decreasing except on `Signal` (derivation-emitted L1-reorg notification); on `Signal`, may regress to a specific derivation-computed target.
  - **S2.c Unsafe:** may regress on explicit `Signal` or on `backup_unsafe_reorg`; never silent.
- **S3. Sticky-flag monotonicity.** `el_sync_finished`, once `true`, remains `true` for the process lifetime. `need_fcu_call_backup_unsafe_reorg` clears exactly once (when the backup FCU succeeds).
- **S4. Task-response partial-commit correctness (bug-class #3803).** Per each `(task × EL response)` pair, the commit policy is defined field-by-field: `Valid` commits everything; `Invalid` commits nothing; `Syncing` commits only fields that satisfy S1+S2 and whose precondition flags are met (e.g. safe/local-safe/finalized on `Syncing` require `el_sync_finished = true`).
- **S5. Engine ↔ CL agreement.** After `ForkchoiceUpdated(head=H) → VALID`, the EL's canonical tip is `H` or a descendant. After `NewPayload(P) → VALID`, the EL has state at `P.hash`. After `INVALID`, the CL commits no head advance implied by that call. Backup-unsafe-reorg is the recovery from an EL that has forgotten an unsafe head.
- **S6. Engine API idempotence.** Sending the same `(newPayload, P)` or `(FCU, S)` twice has the same state effect as sending it once. Retry-on-timeout must not double-commit. **[⇐ S2 + S4 + Option-updates-are-total-assignments]** — kept first-class because it's directly testable at the wire boundary via `FakeEngineClient` call-log inspection.

## D — Derivation state machine

- **D1. At most one `AttributesWithParent` awaiting confirmation.** Enforced by the derivation actor state machine.
- **D2. No derivation before EL sync.** Transition `AwaitingELSyncCompletion → AwaitingL1Data` requires `ELSyncCompleted`.
- **D3. Signal-before-resume.** After `SignalNeeded`, no new attributes may be derived until `SignalProcessed`.
- **D4. Derivation determinism.** Given the same L1 blocks + rollup config + `SystemConfig`, the sequence of `NewAttributesDerived` is byte-identical.
- **D5. Attributes reference a known parent.** Every emitted `AttributesWithParent(A, P)` has `P` equal to the current safe head or a known local-safe extension.

## Sq — Sequencer-only

- **Sq1. No self-reorg of unsafe.** The sequencer never emits an FCU that would move its own unsafe head backward without an L1-triggered `Signal`.
- **Sq2. One payload per slot.** For a given target timestamp, `start_build_block` + `get_sealed_payload` fire at most once. Retries idempotent (S6 restricted to sequencer path).
- **Sq3. Build-parent freshness.** The parent of every payload emitted by `get_sealed_payload` equals `unsafe_head` at the time `start_build_block` was called. On mismatch, the stale build is discarded, not sealed.
- **Sq4. Bounded lag.** `unsafe.number − safe.number < N` where `N` is bounded by channel-timeout / L1-derivation-lookback. If violated, the sequencer is building faster than derivation can catch up.

## V — Validator-only

- **V1. Parent-before-child.** No gossiped or derived payload is applied via `newPayload` before its parent has been applied.
- **V2. Bootstrap consistency.** After `bootstrap_validator()` completes, the CL's `sync_state` heads equal the EL's heads for that safety level. `el_sync_finished` starts `false` and flips to `true` exactly once.

## C — Cross-role choreography

- **C1. Sequencer output ⇒ validator agreement.** If a sequencer produces block `B` and gossips it, an unmodified validator on the same L1 applies `B` and its `latest` equals `B`. **[⇐ D4 + S5 + S1 + V1]**.
- **C2. L1 reorg ⇒ both roles converge.** If L1 reorgs at depth `d`, both sequencer and validator eventually agree on a new safe head derived from the new L1. **[⇐ D3 + D4 + S2.b + L2]**.

## L — Liveness

Time bounds are bounded-lookahead in tests: run `N` extra ticks after all adversarial input stops; assert the property holds.

- **L1. Sequencer never wedges.** From any reachable state, if time advances and a valid parent unsafe head exists, `getPayload` is eventually called and a new unsafe head is produced. No combination of EL responses permanently blocks payload production.
- **L2. Validator never wedges.** From any reachable state, if new payloads are gossiped or derived and time advances, both `unsafe_head` (gossip) and `safe_head` (derivation) reach the available tip.
- **L3. Monotonic-field advance is not silently lost (#3803, generalised).** For every accepted `EngineSyncStateUpdate` setting a monotonic field `F ∈ {safe, local_safe, finalized}` to value `H`, **eventually** `sync_state.F.number ≥ H.number`, provided (a) `H.number ≤ EL's serving tip` at the time of update and (b) the sticky preconditions for `F` hold. Applies field-by-field, independently. **[⇐ S4 + S3 + task-queue retry fairness]**.
- **L4. Confirmations observed by derivation.** After the engine applies attributes `A`, the derivation actor eventually receives `NewAttributesConfirmed(A.parent)`. **[⇐ L3 + actor read fairness]**.
- **L5. No cross-actor deadlock.** The wait-for graph across (engine ⇋ derivation ⇋ network) has no cycle.

## F — Flashblocks (external boundary only)

- **F1. Fold equivalence.** The sum of flashblocks `0..N` for slot `S` folds to the same block hash the sequencer eventually produces for `S`. Internal flashblocks invariants are owned by the flashblocks team.

## Explicitly out of scope

- **JWT freshness / rotation / clock skew** — operational reliability.
- **Peer scoring / gossip transport health** — network-layer.
- **Split-brain resolution across independent sequencers** — system-level (requires L1-authority modeling).
- **Follow-mode** — validator variant, out of scope for this project.
- **Flashblocks internals** — owned by the flashblocks team.

---

## Edge cases (E1–E34)

Each is a specific scenario the properties above must survive. Every edge case names the invariants it exercises. **`E`-numbers are cross-referenced from test function names**: `e14_el_restart_mid_flight` proves E14 holds under this scenario.

### Baseline edge cases (E1–E16)

- **E1. Bootstrap-from-fresh.** Node starts with all heads at genesis, `el_sync_finished = false`. Exercises: V2, S1, S3.
- **E2. Reorg during derivation-in-flight.** Derivation emitted `AttributesWithParent(A)` on old L1; L1 signal arrives before engine confirms. Exercises: D1, D3, D4.
- **E3. Empty L1 batch.** L1 block contains no L2 batches. Safe head's L1-origin advances but L2 number does not. Exercises: S2.b.
- **E4. Sequencer crash mid-slot.** Payload build started but sealed envelope never retrieved. On restart, exactly one payload is produced for the slot. Exercises: Sq2, S6.
- **E5. Concurrent gossip + derivation of same block.** Validator receives block N via gossip while derivation is producing it. Applied exactly once. Exercises: V1, D4, S5.
- **E6. Deep L1 reorg past safe head but not past finalized.** Safe regresses to a valid derivation state; finalized untouched. Exercises: S2.a, S2.b, D3.
- **E7. Deep L1 reorg past finalized.** Should never happen (L1 finality assumption); if it does, the CL halts/crashes rather than silently proceeding.
- **E8. Task-queue backpressure.** Many tasks enqueued before the first completes. No state effect lost or applied twice; ordering preserved. Exercises: S4, S6.
- **E9. Backup-unsafe-reorg race.** EL forgot the unsafe head; CL sets `need_fcu_call_backup_unsafe_reorg` and issues recovery FCU; meanwhile a gossip block arrives on the forgotten chain. Sticky flag clears exactly once. Exercises: S3, S5.
- **E10. `Syncing` EL response with `el_sync_finished = false` on a composite update including `safe`.** The exact #3803 case. Nothing that requires `el_sync_finished = true` commits until the flag flips. Exercises: S4, L3.
- **E11. `INVALID newPayload` on an ancestor of currently-committed safe.** Contradiction between prior VALID and current INVALID. CL detects, halts or rolls back deterministically. Exercises: S5.
- **E12. Repeated `SignalNeeded` before `SignalProcessed` observed.** Two L1 reorgs in quick succession. Derivation collapses to the deepest signal. Exercises: D3.
- **E13. L1 provider stall.** `BeaconClient` / `L1RetrievalProvider` stops responding. Derivation halts on `AwaitingL1Data`; unsafe head keeps advancing from gossip. Exercises: L2, S1.
- **E14. EL restart mid-flight.** EL process killed and restarted; reports `Syncing` on every FCU/newPayload until state rebuilt. `el_sync_finished` does *not* regress. Exercises: S3, S5, L3.
- **E15. Fresh / pruned EL on startup with CL SafeDB checkpoints.** Anchor bug #2 in edge-case form. `NoopForkchoiceCheckpointReader` ⇒ deterministic sync-start failure; wired reader ⇒ successful recovery. Exercises: startup-liveness, S1, S5.
- **E16. `find_earliest_unpruned_block` binary-search corner cases.** Anchor #3 (PR #2967). Search terminates at an unpruned block ≤ latest, or returns "no unpruned block found" — never re-raises `MissingL1InfoDeposit`.

### Syncing-stall edge cases (E17–E34)

`Syncing` from the EL is the response class that hides #3803-shaped bugs: the CL thinks it made progress, the EL disagrees, and the CL must correctly commit — or defer — the composite update. Every case's property is a **liveness statement**.

**Group A — Task-queue siblings of #3803 (test S4 in the other tasks).**

- **E17 (S-A1). `ConsolidateTask` + `Syncing`.** Composite update from Consolidate is preserved across a `Syncing` response.
- **E18 (S-A2). `FinalizeTask` + `Syncing`. HIGHEST SEVERITY.** Finalized advance under `Syncing` — must never regress S2.a. Bug shape: dropped finalized advance ⇒ silent regression ⇒ potential double-finality.
- **E19 (S-A3). `InsertTask` + `Syncing`.** Block is either committed on next `Valid` or requeued; not silently lost.

**Group B — Sequencer stalls (test L1).**

- **E20 (S-B1). Sequencer FCU-with-attrs → `Syncing`.** `payload_id` handling; sequencer never waits forever.
- **E21 (S-B2). getPayload race after FCU-with-attrs succeeded but EL now `Syncing`.** Slot number monotonically advances; sequencer does not wedge.
- **E22 (S-B3). Sequencer's own newPayload → `Syncing`.** Sequencer re-issues FCU, waits for Valid, proceeds.

**Group C — Recovery-path Syncing (test S3 sticky-flag monotonicity).**

- **E23 (S-C1). Backup-unsafe-reorg FCU → `Syncing`.** `need_fcu_call_backup_unsafe_reorg` remains `true`; recovery FCU keeps firing.
- **E24 (S-C2). L1-reorg signal-reset FCU → `Syncing`.** Reset commits or retries; not silently dropped.
- **E25 (S-C3). Restart mid-Syncing.** Recovered forkchoice consistent with pre-crash state; no forkchoice regression across restart.

**Group D — Chronic / adversarial patterns.**

- **E26 (S-D1). Long Syncing chain N=100+.** Bounded work per tick; no retry storm; state remains sound.
- **E27 (S-D2). Alternating Valid ↔ Syncing.** `el_sync_finished` sticky-true (S3) across every flip.
- **E28 (S-D3). Same task retried after Syncing with identical input.** S6 idempotence under `Syncing` specifically.

**Group E — Composite-update degenerate cases.**

- **E29 (S-E1). Composite, all 4 fields set + `Syncing`.** Maximum-composite. `unsafe` never commits; other three commit only if at-or-behind unsafe.
- **E30 (S-E2). Composite with ONLY finalized set + `Syncing`.** Minimal case. Under-tested corner.
- **E31 (S-E3). Changing `latestValidHash` across two Syncing responses.** CL commits nothing on either; ignores `latestValidHash` on `Syncing`.

**Group F — Cross-actor Syncing interactions.**

- **E32 (S-F1). Validator newPayload (gossip path) → `Syncing`.** Validator retries or defers; unsafe eventually applied.
- **E33 (S-F2). Syncing + L1 provider stall (dual failure).** Recovery order well-defined; neither failure permanently masks the other.
- **E34 (S-F3). Derivation-observer view of #3803.** L4 — every applied-attrs eventually produces `NewAttributesConfirmed`.

---

## Test-file map

| Property class | Test file | Package |
|---|---|---|
| Anchor #1 (#3803) — 16 cases | `crates/consensus/engine/src/task_queue/tasks/synchronize/task_test.rs` → `tests::anchor_3803` | `base-consensus-engine` |
| Anchor #2 (#2698) — 2 cases | `crates/consensus/engine/src/sync/mod.rs` → `tests::anchor_2698_*` | `base-consensus-engine` |
| Anchor #3 (#2967) — 4 cases | `crates/consensus/engine/src/sync/forkchoice.rs` → `tests::anchor_2967_*` | `base-consensus-engine` |
| Harness scaffolding + smoke | `test_utils/driver.rs` → smoke test | `base-consensus-node` |
| E1, E2, E3, E5, E8, E9, E13, E14 | `test_utils/edge_cases.rs` | `base-consensus-node` |
| D4, D5, L5, V2 (+ ignored: D1, L4, V1) | `test_utils/invariant_tests.rs` | `base-consensus-node` |
| S1, S2, S3, S4, S5, S6, D2, D3, L1, L2, L3 | `test_utils/proptest_model.rs` | `base-consensus-node` |
| C1, C2, liveness gate | `test_utils/two_node.rs` | `base-consensus-node` |
| E17–E24 (Syncing wave 1, S-A/B/C, 4 passing + 3 harness-gap ignored) | `test_utils/syncing_stalls.rs` | `base-consensus-node` |
| E22, E26–E31 (Syncing wave 2, S-B3, S-D, S-E) | `test_utils/syncing_stalls_wave2.rs` | `base-consensus-node` |
| Sq1–Sq4 + E4, E6, E7, E11, E12 | `test_utils/sequencer_and_reorg.rs` | `base-consensus-node` |
| F1 (flashblocks fold) | `crates/execution/flashblocks-node/tests/f1_fold_equivalence.rs` | `base-flashblocks-node` |

---

## Known remaining ignored tests (post harness upgrade)

The Tier-0 harness plumbing gaps for FCU attrs, `new_payload_v3`, `get_payload_v3`, derivation-state observation, and backup-unsafe-reorg flag forcing are now closed. The following tests remain intentionally `#[ignore]`d because they still require broader product/lifecycle seams beyond test-utils-only work.

| Test | File | Why still ignored |
|---|---|---|
| `e4_sequencer_crash_mid_slot` | `sequencer_and_reorg.rs` | Needs an exposed SequencerActor lifecycle seam around in-flight slot build/getPayload restart semantics. |
| `e7_deep_l1_reorg_past_finalized` | `sequencer_and_reorg.rs` | Requires injecting impossible past-finalized reorg/reset behavior not exposed in frozen Tier-0 harness. |
| `e11_invalid_newpayload_on_committed_safe_ancestor` | `sequencer_and_reorg.rs` | Needs contradiction-handling observability (halt/rollback outcome) beyond current test-utils handles. |
| `e12_repeated_signal_needed_before_processed` | `sequencer_and_reorg.rs` | Needs pre-processed `SignalNeeded` queue-depth observability; current seam only injects processed signals. |
| `s_d1_long_syncing_chain_100_no_retry_storm` | `syncing_stalls_wave2.rs` | Needs deterministic, directly observable Syncing→Valid retry-flip path over long windows. |
| `s_e1_composite_all_four_fields_syncing` | `syncing_stalls_wave2.rs` | Needs direct injection/observation of all-four-fields `EngineSyncStateUpdate` composite path. |
| `s_e2_composite_finalized_only_syncing` | `syncing_stalls_wave2.rs` | Needs direct finalized-only composite-update seam (both parameterized branches). |

**Harness close-out checklist status:**

1. ✅ `EngineClientCall::ForkChoiceUpdatedV3` now records `payload_attributes`.
2. ✅ `FakeEngineClient` scripts + records `new_payload_v3` calls.
3. ✅ `FakeEngineClient` scripts + records `get_payload_v3` calls.
4. ✅ Test handles expose derivation state via `CurrentStateRequest` oneshot query.
5. ✅ Test handles can set `need_fcu_call_backup_unsafe_reorg` through engine actor request queue.
