# S2 simulation entrypoint and durable record design

Status: proposed for independent review. This document specifies implementation; it contains no product-code change and performs no operational action.

## 1. Scope and measured starting point

At base `feb8561b`, `SimBackend` and `send_gated` exist, but no production code calls `send_gated` and no `SimulationRecord` is persisted. The node has only one feature path into the arm tier:

```text
base-reth-node/arm-live-egress
  -> base-execution-cli/arm-live-egress
  -> mev-trader-submit/arm-live-egress
  -> mev-trader-submit/arm
```

Consequently, compiling the safe simulation backend currently also compiles the live transport. S2 first creates a simulation-only rung, then installs the first bounded production submission worker and a local durable record ledger.

S2 MUST NOT introduce a second candidate, assembly, economics, authorization, signing, or request-building path. It MUST NOT provision suppression, create owner signatures, invent a drawdown value, open a socket from simulation or persistence, activate the MEV trader in a default node, or perform any deployment/restart/funding/submission operation.

## 2. Feature graph and broadcastability

The feature graph becomes:

```text
base-reth-node/arm-sim
  -> base-execution-cli/arm-sim
  -> base-execution-cli/t4e-handoff
  -> mev-trader-submit/t4e-handoff
  -> mev-trader-submit/arm

base-reth-node/arm-live-egress
  -> base-reth-node/arm-sim
  -> base-execution-cli/arm-live-egress
  -> mev-trader-submit/arm-live-egress
```

`base-execution-cli/arm-sim` explicitly includes both `t4e-handoff` and `mev-trader-submit/arm`. The explicit arm edge is sealed even though `t4e-handoff` also reaches arm transitively. This makes the safe rung reviewable by manifest inspection and gives it the existing candidate-to-witness handoff needed by the entrypoint.

`arm-live-egress` is changed to include `arm-sim` before adding the outer live feature. This guarantees simulation and live use the same worker and handoff wiring; live does not gain a separate entrypoint.

All three features remain default-off. The default node still has no `mev-trader-submit` dependency in its resolved graph. The workspace inverse tree still resolves only `mev-trader-submit/default`. An `arm-sim` node contains `arm` and `t4e-handoff`, but contains none of `arm-live-egress`, `ProdBackend`, `BackendPermit::Live`, `LiveEgressPermit`, `reqwest`, or a socket-opening call.

The existing `--mev-live-egress` flag remains compiled only with `arm-live-egress`; it is absent from an `arm-sim` binary. An arm-sim build always reduces runtime backend selection to `RuntimeBackend::simulated(&SimBackend)`.

Compiling `arm-sim` is capability selection, not implicit runtime activation. The existing exact MEV trader/T4a/T4b/T4d activation gates and Flashblocks requirement remain authoritative. S2 adds no environment alias and no default-on startup path.

## 3. One unified data path

The only admitted flow is:

```text
existing candidate selection
  -> existing T4b assembler
  -> existing PriorityEconomicsAuthority + economics::evaluate
  -> existing SealedUnsignedCandidate
  -> existing T4e by-value handoff
  -> InstalledSubmissionBridge::into_checked_candidate
  -> existing suppression/G7/live/deployment/R9 witness conjunction
  -> existing custody load_and_sign
  -> PairedSubmission::assemble
  -> send_gated(RuntimeBackend::simulated)
  -> SubmitOutcome::Simulated(SimulationRecord)
  -> bounded durable projection
```

The T4d authority and worker share one `Arc<InstalledSubmissionBridge>`. `t4d_shadow::observer` changes its bridge ownership from a value to an `Arc`, retaining one clone and handing one clone to the S2 installer. This preserves the installation seal: the worker can convert only a candidate minted by that same bridge installation.

The current `T4eCandidateHandoff` remains the sole candidate boundary. S2 installs its bounded sink; it does not add a second observer or tap. `try_handoff` remains non-blocking and by-value.

### 3.1 Missing production authorities are typed failures, never substitutes

`ProductionDrawdownSource`, `ProductionCodeHashProvider`, and `ProductionDeploymentIdentitySource` already exist as generic provider adapters. What is missing is the node-binding layer beneath them: every current `DrawdownAuthority` and `CommittedStateAuthority` implementation is test-only, there is no production importer of campaign G7/live/deployment attestations, and there is no production installer for the attested R9 store. In particular, S1-b's L4 canonical-balance check has no production authority implementation today and therefore cannot yet run in production. S2 does not rebuild the existing provider adapters or fill the missing authorities with zero drawdown, unsigned JSON, an RPC lookup, a second witness path, or test fixtures.

The high-level worker accepts only an installed authority bundle made from the existing verified types. Its production installer reports a closed `SimulationEntrypointUnavailable` reason for each absent prerequisite:

- `ArmRuntimeUnavailable(ArmRuntimeOpenError)`;
- `DrawdownAuthorityUnavailable`;
- `CampaignAttestationsUnavailable`;
- `ClaimStoreUnavailable`;
- `DeploymentIdentityUnavailable`;
- `PersistenceUnavailable(SimulationStoreOpenError)`.

In particular, absent `/home/ubuntu/.config/mev-suppression.json` reaches `ArmRuntimeUnavailable` and does not panic or disappear into an `Option`. The node remains up, installs a rejecting T4e sink, emits one structured status with the typed reason, and exposes the same typed status to tests/diagnostics. Every candidate handed to that sink terminates as `T4eHandoffError::Rejected`; it is never retained or retried.

The worker implementation contains the first non-test `send_gated` callsite, but it can reach that call only after the existing verified witness has produced a `PairedSubmission`. Until owner-reviewed producers for the currently missing authorities are installed, the production node is observably unavailable rather than pretending to simulate. This is the required interpretation of “if a piece is missing, say so rather than building a parallel one.” S2 tests the ready path with existing verified synthetic authorities; product code contains no fixture constructor.

## 4. Thread ownership, bounds, and shutdown

`send_gated` is driven by one named dedicated OS thread, `base-mev-arm-egress`. It is created with `std::thread::Builder`, not `tokio::spawn`, `spawn_blocking`, `block_in_place`, an ExEx callback, or a consensus task. The thread owns:

- the installed authority bundle and shared bridge;
- the selected `RuntimeBackend` and `SimBackend` (and, only in a live build, the selected `ProdBackend`);
- the R9 claim-store writer handle;
- the durable simulation ledger writer.

The T4e sink owns a `std::sync::mpsc::SyncSender` with capacity exactly one and uses `try_send`. Full maps to `T4eHandoffError::Busy`; disconnected maps to `Closed`; unavailable installation maps to `Rejected`. The caller never waits for signing, disk fsync, or network I/O. Together with T4d’s existing one-candidate slot, at most two sealed candidates exist across the drain/worker boundary; no `Vec`, unbounded channel, retry queue, or cloned candidate is introduced.

The worker processes exactly one candidate to a terminal result before receiving another. A live build uses the same thread and queue, so blocking `reqwest` can never migrate onto Tokio or ExEx.

On graceful shutdown the T4d observer closes first, the sender is dropped, and the worker drains at most the one already accepted candidate. The node’s graceful-shutdown owner joins the OS thread through its existing blocking-shutdown lane. A persistence failure, poison, or structurally unknown outcome closes the worker; later handoffs see `Closed`, not silent loss. Ledger closure uses the distinct operator-visible status described in §7 rather than an ordinary candidate rejection.

## 5. Economics retention and correlation

The current positive-EV gate computes a `PriorityFilterDecision` and discards it after admission. S2 renames/exposes that immutable checked output internally as `PriorityEconomicsReceipt` and retains it by value in `ValidatedUnsignedAtomicTx`. The receipt follows the existing linear transaction through `SealedUnsignedCandidate`, `CheckedCandidate`, `ValidatedExecutionIdentity`, signed submission, egress plan, and `SimulationRecord`. No evaluator is rerun and no scalar is recomputed along the transport path.

The retained receipt contains the already checked values:

- gross profit;
- kickback;
- retained value;
- execution gas estimate;
- L2 execution fee;
- L1 data fee;
- total cost;
- strictly positive expected EV;
- candidate/economics authority block and base/priority/max fee inputs required to reproduce the calculation.

A `SimulationCorrelationKey` is:

```text
keccak256(
  "base-mev/simulation-correlation/v1" ||
  campaign_id || victim_tx_hash || plan_digest || signed_tx_hash
)
```

All components come from the already validated identity and signed transaction. The key is computed once when the paired submission is assembled and carried into the simulation record. The durable record also stores those unhashed join fields separately. This supports joins against the candidate ledger, canonical transaction receipts, victim transaction, and route plan without treating a hash as reversible data.

## 6. What is persisted

A simulated attempt has no observed inclusion outcome. The durable schema therefore calls it an `attempt`, never a successful outcome. It stores enough immutable input/economics evidence for offline expected-EV recomputation and enough correlation data for a later canonical-chain resolver to determine inclusion and realised EV.

`SimulationDurableRecordV1` contains only bounded scalar/enum/fixed-byte data:

- schema version and monotonically increasing ledger sequence;
- prior-record hash; the current canonical-record hash is retained in the durable `head` anchor;
- correlation key;
- simulation attempt kind (`Initial` or `AttributionRetry`);
- campaign ID, victim hash, plan digest, parent/block identity, and signed transaction hash;
- executor, sender, nonce, gas limit, max fee, priority fee, base fee, and validity deadline;
- two protocol/adapter/runtime-hash identities;
- all `PriorityEconomicsReceipt` fields;
- expected inclusion hash and request-channel count.

Raw transaction bytes, request JSON bodies, Blink credential, endpoint URL, key material, arbitrary strings, and unbounded vectors are not persisted. The signed transaction hash is enough to resolve inclusion; the economics receipt is enough to recompute expected EV. Realised EV is deliberately not fabricated by S2: an offline resolver joins this attempt to canonical receipts/traces using signed transaction hash plus candidate correlation fields.

## 7. Durable bounded ledger

S2 does not reuse `EdgeCanonicalWriterV1` directly. That type is a private CLI-owned edge campaign coordinator with registry joins, multiple ledgers, periodic accounting, cutoff publication, and edge-specific provenance. Coupling the arm worker to it would import a parallel measurement runtime and make `arm-sim` depend on `edge-measurement`. Instead S2 reuses its reviewed durability pattern: exclusive output lease, strict startup scan, canonical bytes, hash chaining, write/sync/atomic publish/directory sync, and fail-closed recovery. The hash chain is load-bearing because these records feed the P2 go/no-go decision: it detects accidental deletion, reordering, partial archival, and corruption, while explicitly providing no authentication against an adversary able to rewrite the entire unsigned ledger.

The simulation ledger uses the compile-pinned directory:

```text
/home/ubuntu/.local/state/base-mev/simulation-v1
```

There is no CLI path, environment override, network sink, relay, callback, or trait object capable of exporting records.

The bound is 262,144 records, each with a canonical encoded maximum of 16 KiB: at most 4 GiB of record payload, plus fixed filesystem metadata. This covers more than two attempts per two-second Base block for a 72-hour measurement campaign (129,600 blocks) and leaves no ambiguity that 4,096 was merely a test-scale number. The ledger never overwrites or deletes a published record. At sequence 262,144 it returns typed `Full` before R9 claim/signing and closes admission. There is no in-memory retention beyond the candidate currently owned by the worker and one fixed-size encoding buffer.

Each record is published as one sequence-addressed file:

```text
00000000000000000042.open
00000000000000000042.record
```

Publication is:

1. strict bounded canonical encoding in memory;
2. create-new `.open` in the private pinned directory with mode `0600`;
3. `write_all`, then `sync_all` on the file;
4. publish `.record` with a same-directory hard link, which fails if that sequence already exists, then remove `.open`;
5. `sync_all` on the directory;
6. write and sync a fixed 40-byte `head.open` containing `next_sequence || latest_record_hash`, atomically replace `head`, and sync the directory again;
7. only then report `Persisted` and accept the next candidate.

`head` is the durable external anchor for the unsigned record chain. The startup scan must equal both its sequence and latest hash, so deleting even the trailing record or the complete record set is visible rather than becoming a shorter apparently valid chain.

Startup takes an exclusive non-blocking directory lease, rejects non-regular/hard-linked published entries, unknown names, duplicate sequences, gaps, unknown schema versions, oversize records, hash-chain/head mismatch, and a pre-existing `.open` (including `head.open`). It does not guess, truncate, delete, skip, or `unwrap_or` malformed state. An empty existing/private directory is sequence zero and receives a zero head. Directory creation is allowed only at the compile-pinned path with mode `0700`; it is persistence setup, not arming or suppression provisioning.

Capacity is preflighted before the irreversible R9 claim. Any I/O error after preflight but before durable publication is a typed `PersistenceFailed`; because the claim/signing outcome may already be irreversible, the worker closes and does not retry that candidate.

### 7.1 Operator-visible closure and recovery

Ledger shutdown is not emitted as an ordinary `BridgeRejected`, `Busy`, or `Closed` candidate result. The entrypoint publishes one sticky `SimulationEntrypointStatus::LedgerClosed` value and one structured `error!` event. Its closed reason is total and distinguishable:

- `Full { ledger_epoch, next_sequence: 262144, capacity: 262144 }`;
- `WriteFailed { ledger_epoch, next_sequence, operation, io_kind }`;
- `InvalidExistingLedger { ledger_epoch: Option<_>, class }`.

`operation`, `io_kind`, and `class` are bounded enums, not interpolated paths or arbitrary error strings. The status remains queryable for the process lifetime, so an operator can distinguish capacity exhaustion, a transient-looking write failure, and structural corruption after the original log event. The T4e sink then reports `Closed`; it never downgrades the sticky ledger reason.

S2 performs no automatic rotation, deletion, truncation, in-place repair, or retry. Recovery is an explicit owner act:

1. stop the node and preserve the complete pinned directory;
2. inspect capacity/disk/filesystem health and archive the directory under an owner-chosen immutable location without modifying its records;
3. for `InvalidExistingLedger`, retain the invalid directory for forensic review rather than repairing it;
4. move the entire old directory away atomically, create a new empty private pinned directory, and restart the explicitly built arm-sim node;
5. verify the startup status reports a new random `ledger_epoch` and sequence zero before resuming the campaign.

The new epoch is generated from OS randomness when an empty ledger is initialized, durably stored before record admission, included in every record/correlation envelope, and prevents cross-rotation sequence confusion. If the owner does not perform this act, the correct recovery is “remain stopped and escalate”; there is no hidden reset surface. S2 documents this procedure but does not execute it, add a reset command, or treat restart alone as recovery.

## 8. Typed terminal results

The worker classifies every accepted candidate exactly once:

- `Persisted { sequence, correlation_key }`;
- `BridgeRejected`;
- `SuppressionClosed`;
- `ProofUnavailable`;
- `ClaimAlreadyExists`;
- `ClaimFailedStop`;
- `AuthorizationClosed`;
- `SigningFailedStop`;
- `FreshnessClosed`;
- `PersistenceFull`;
- `PersistenceFailed`;
- `UnexpectedLiveOutcome`.

In an arm-sim build, the only accepted transport outcome is `SubmitOutcome::Simulated`. `LiveComplete`, live partial failures, `LiveLocksClosed`, and any live permit are structurally absent or classified as `UnexpectedLiveOutcome` and close the worker. `NoEgress` maps to `FreshnessClosed`. No `_ =>`, `continue`, or invented default admits an outcome.

## 9. Seals and committed mutants

Validators consume strict `serde_json::Value` fixture objects or parsed Rust/Cargo syntax. They reject unknown/missing/duplicate keys, duplicate target-scoped dependency entries, unknown feature members, unclassified terminal values, and source parse failures.

Committed controls/mutants:

- `S0 GREEN`: exact arm-sim feature chain contains `arm`/T4e and no live feature;
- `S1 RED`: add `arm-live-egress` to either arm-sim manifest rung;
- `S2 RED`: add `reqwest` to the arm-sim resolved closure fixture;
- `S3 RED`: make the entrypoint feature/default reachable in the default node;
- `E0 GREEN`: one candidate follows assembler/economics/witness/send_gated in order;
- `E1 RED`: bypass or remove the `PriorityEconomicsAuthority`/checked economics receipt edge;
- `E2 RED`: add a second candidate/assembler/evaluator path;
- `E3 RED`: remove either same-installation bridge check;
- `E4 RED`: construct `LiveEgressPermit` or select `ProdBackend` in simulation;
- `Q0 GREEN`: capacity-one `sync_channel` plus non-blocking `try_send` and one OS worker;
- `Q1 RED`: unbounded channel, capacity above one, Tokio/ExEx execution, or blocking `send` at handoff;
- `P0 GREEN`: strict V1 record at the byte/record maximum publishes and verifies;
- `P1 RED`: add any socket/network/HTTP/relay dependency or call to persistence;
- `P2 RED`: exceed record count or encoded-size bound;
- `P3 RED`: unknown field/version, sequence gap/duplicate, stale `.open`, non-regular/hard-linked path, or hash mismatch;
- `P4 RED`: omit file fsync, atomic no-replace publication, or directory fsync;
- `P5 RED`: remove correlation key or any economics scalar required for recomputation;
- `U0 GREEN`: absent suppression yields typed `ArmRuntimeUnavailable` and a rejecting sink while node startup succeeds;
- `U1 RED`: panic, silent `Option`/no-op, fixture fallback, zero drawdown, or candidate retention when runtime/authority is unavailable.

Every mutant first asserts its patch changed the input. At least `S0`, `E0`, `Q0`, `P0`, and `U0` stay GREEN.

Existing seals are not weakened. Expected explicit amendments are limited to:

1. CLI/node feature allowlists gain the exact `arm-sim` rung and live features compose it;
2. the T4e handoff surface gains the reviewed S2 installer/entrypoint facade;
3. the economics/validated-transaction allowlists gain the immutable receipt and correlation accessors;
4. the arm root allowlist gains only the high-level simulation entrypoint/status/store record types required by CLI wiring;
5. arm backend, live permit, reqwest, socket-site, T4b, linker, and exact PR#48/#50 default-closure assertions remain intact.

Each amendment is listed in the implementation PR body with before/after counts.

## 10. Implementation placement

Meaningful logic remains outside `lib.rs` and binary glue:

- submit crate: arm simulation entrypoint/worker, typed statuses, correlation projection, bounded store, economics receipt propagation, seals/tests;
- CLI trader module: committed-state/drawdown/proof authority installation, shared bridge handoff installation, structured unavailable status, graceful worker ownership;
- CLI/node manifests: exact feature forwarding only;
- node binary: no meaningful logic.

No generic persistence framework or network sink abstraction is introduced.

## 11. Verification

The implementation PR reports literal output for:

```text
cargo tree -p base-reth-node --features arm-sim -e features -i mev-trader-submit
cargo tree -p base-reth-node --features arm-sim
cargo tree --workspace -i mev-trader-submit -e features
cargo tree -p base-reth-node -i mev-trader-submit
```

The second output is inspected structurally and `reqwest` occurrence count is reported as zero without changing the command’s tree. Required suites are:

- submit lib under `arm` and `arm-sim` closure;
- `arm_capability_seal` under arm and live configurations;
- `t4b_capability_seal`;
- linker `capability_seal` mutants;
- `base-mev-trader` capability seal and full suite;
- CLI entrypoint/persistence integration tests;
- serial mutant run with one PASS/FAIL line per named control/mutant.

No live/provisioning binary is executed. Neither PR is merged by the author.

## 12. Acceptance criteria

S2 is complete when an explicitly built arm-sim node has a bounded, non-blocking path from the existing T4e handoff to a dedicated OS worker; the worker contains the first production `send_gated` caller and durably publishes a bounded, joinable, EV-recomputable simulation attempt only after the existing unified witness succeeds; all unavailable prerequisites are typed and visible; persistence failure is fail-closed; and default/workspace artifacts retain broadcastability zero.
