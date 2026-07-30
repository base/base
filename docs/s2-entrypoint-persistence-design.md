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

Consequently, compiling the safe simulation backend currently also compiles the live transport. PR #52 creates the simulation-only rung and supplies the bounded worker/library entrypoint, typed unavailable sink, and local durable record ledger. It deliberately does not install the first production caller of `send_gated`: the current production handoff is Rejected-only and cannot produce `Busy`, `Closed`, or `Ready`.

S2 MUST NOT introduce a second candidate, assembly, economics, authorization, signing, or request-building path. It MUST NOT provision suppression, create owner signatures, invent a drawdown value, open a socket from simulation or persistence, activate the MEV trader in a default node, or perform any deployment/restart/funding/submission operation.

The complete production installation is deferred to the exactly named follow-up **Production T4e Simulation Installation + Settled-Loss Authority**. The detailed worker, persistence, shutdown, and seal requirements below remain the normative design for that follow-up; descriptions of its ready path are completion requirements, not claims that PR #52 or current S2 is production-reachable.

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

The only admitted flow after **Production T4e Simulation Installation + Settled-Loss Authority** is complete will be:

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

The completed installer MUST give T4d authority and T4e worker one shared `Arc<InstalledSubmissionBridge>`. `t4d_shadow::observer` must retain one clone and hand one clone to the production T4e installer. This preserves the installation seal: the worker can convert only a candidate minted by that same bridge installation.

The current `T4eCandidateHandoff` remains the sole candidate boundary. PR #52 supplies its bounded worker/library sink implementation and the node-facing typed unavailable sink; it does not add a second observer or tap and does not connect a production candidate to the worker. `try_handoff` remains non-blocking and by-value. Until the named follow-up is complete, every production handoff terminates as `T4eHandoffError::Rejected`.

### 3.1 Missing production authorities are typed failures, never substitutes

`ProductionDrawdownSource`, `ProductionCodeHashProvider`, and `ProductionDeploymentIdentitySource` already exist as generic provider adapters. At PR #52's `feb8561b` base, the node-binding layer beneath them is absent: every `DrawdownAuthority` and `CommittedStateAuthority` implementation is test-only, there is no production importer of campaign G7/live/deployment attestations, and there is no production installer for the attested R9 store. PR #55 separately adds only the committed-state adapter; it does not add the remaining conjunction. PR #52 does not rebuild the existing provider adapters or fill missing authorities with default zero drawdown, unsigned JSON, an RPC or network lookup, a second witness path, or test fixtures.

PR #52 does not probe a partial prerequisite set or imply that one successful probe makes the installation ready. Its node-facing sink reports exactly `SimulationEntrypointUnavailable::ProductionInstallationDeferred`, emits one structured status naming **Production T4e Simulation Installation + Settled-Loss Authority**, and rejects every candidate with `T4eHandoffError::Rejected`.

The completed follow-up installer must replace that aggregate deferral with a closed typed reason for each failed prerequisite:

- `ArmRuntimeUnavailable(ArmRuntimeOpenError)`;
- `DrawdownAuthorityUnavailable`;
- `CampaignAttestationsUnavailable`;
- `ClaimStoreUnavailable`;
- `DeploymentIdentityUnavailable`;
- `PersistenceUnavailable(SimulationStoreOpenError)`.

In particular, the completed installer must map an absent `/home/ubuntu/.config/mev-suppression.json` to `ArmRuntimeUnavailable` rather than panic or erase the failure behind an `Option`. PR #52 tests the library ready path with existing verified synthetic authorities, but product code contains no fixture constructor, no first production `send_gated` callsite, and no production path that can report `Ready`, `Busy`, or `Closed`.

### 3.2 Auditable production-installation deferral

The first production caller of `send_gated` is deferred in full to **Production T4e Simulation Installation + Settled-Loss Authority**. That follow-up is complete only when all of these concrete prerequisites are installed and verified together:

- the PR #55 committed-state adapter;
- an authoritative settled-loss `DrawdownAuthority`;
- verified campaign, G7, live, and deployment proofs;
- an identity-matched `VictimClaimStore`;
- pinned custody, fail sink, and arming criteria;
- the compile-pinned `SimulationStore`;
- one shared `Arc<InstalledSubmissionBridge>` between T4d and T4e.

PR #55 alone is insufficient: its committed-state adapter cannot establish settled loss, proof provenance, claim-store identity, custody/fail behavior, arming, persistence, or same-installation bridge identity. The production installer MUST reject installation unless the entire conjunction is present and identity-matched. It MUST NOT fake `Ready`, use default zero for any authority, or add an RPC, network, fixture, or other fallback.

The completed installer MUST reserve bounded worker admission before any R9 claim or signing. Its handoff mapping is exact: `Busy` means admission occupied, `Closed` means worker closed or disconnected, and `Rejected` means unavailable installation. `Ready` becomes valid only after the complete prerequisite conjunction and shared bridge are installed. Until then the current production contract is deliberately Rejected-only.

## 4. Thread ownership, bounds, and shutdown

In the completed production installation, `send_gated` is driven by one named dedicated OS thread, `base-mev-arm-egress`. It is created with `std::thread::Builder`, not `tokio::spawn`, `spawn_blocking`, `block_in_place`, an ExEx callback, or a consensus task. The thread owns:

- the installed authority bundle and shared bridge;
- the selected `RuntimeBackend` and `SimBackend` (and, only in a live build, the selected `ProdBackend`);
- the R9 claim-store writer handle;
- the durable simulation ledger writer.

The PR #52 worker/library sink owns a `std::sync::mpsc::SyncSender` with capacity exactly one and uses `try_send`. In the future production installation, full maps to `T4eHandoffError::Busy`, disconnected maps to `Closed`, and unavailable installation maps to `Rejected`; current production exposes only the last mapping. The caller never waits for signing, disk fsync, or network I/O. Admission MUST be successfully reserved before R9 claim/signing begins. Together with T4d’s existing one-candidate slot, at most two sealed candidates exist across the drain/worker boundary; no `Vec`, unbounded channel, retry queue, or cloned candidate is introduced.

The installed worker processes exactly one candidate to a terminal result before receiving another. A live build uses the same thread and queue, so blocking `reqwest` can never migrate onto Tokio or ExEx.

After production installation, graceful shutdown closes the T4d observer first, drops the sender, and lets the worker drain at most the one already accepted candidate. The node’s graceful-shutdown owner joins the OS thread through its existing blocking-shutdown lane. A persistence failure, poison, or structurally unknown outcome closes the worker; later handoffs see `Closed`, not silent loss. Ledger closure uses the distinct operator-visible status described in §7 rather than an ordinary candidate rejection.

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

The durable correlation surface is the fixed `SimulationCorrelationEnvelopeV1 { ledger_epoch: [u8; 32], sequence: u64, correlation_key: [u8; 32] }`, not a bare correlation key. `ledger_epoch` is not an input to the V1 correlation-key hash; it names the ledger generation in which that key was admitted. Every persisted record, `Persisted` terminal result, and operator-facing correlation export carries this envelope. Decoding rejects a missing, duplicate, or wrongly sized epoch, an envelope epoch unequal to the record and durable head epochs, an envelope sequence unequal to the record sequence, or a correlation key that does not recompute from the record's unhashed join fields.

## 6. What is persisted

A simulated attempt has no observed inclusion outcome. The durable schema therefore calls it an `attempt`, never a successful outcome. It stores enough immutable input/economics evidence for offline expected-EV recomputation and enough correlation data for a later canonical-chain resolver to determine inclusion and realised EV.

`SimulationDurableRecordV1` contains only bounded scalar/enum/fixed-byte data:

- `schema_version`, `ledger_epoch: [u8; 32]`, and monotonically increasing `sequence: u64`;
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

The durable head-anchor schema is `SimulationLedgerHead { ledger_epoch: [u8; 32], next_sequence: u64, latest_record_hash: [u8; 32] }`. Its canonical encoding is exactly 72 bytes: the epoch, the big-endian sequence, then the hash; no field is optional.

Publication is:

1. strict bounded canonical encoding in memory;
2. create-new `.open` in the private pinned directory with mode `0600`;
3. `write_all`, then `sync_all` on the file;
4. publish `.record` with a same-directory hard link, which fails if that sequence already exists, then remove `.open`;
5. `sync_all` on the directory;
6. encode that fixed 72-byte schema into `head.open`, write and sync it, atomically replace `head`, and sync the directory again;
7. only then report `Persisted` and accept the next candidate.

`head` is the durable external anchor for the unsigned record chain. The startup scan must equal both its sequence and latest hash, so deleting even the trailing record or the complete record set is visible rather than becoming a shorter apparently valid chain.

`ledger_epoch` is a non-zero 32-byte value generated from OS randomness exactly once when the process creates a new pinned ledger directory. Before admission opens, initialization durably publishes a 72-byte `head` containing that epoch, sequence zero, and the all-zero latest hash, using the same file-sync, atomic-replace, and directory-sync discipline. Each record stores the identical epoch, and every later head update preserves it byte-for-byte. Record encoding and startup validation reject a missing or wrongly sized epoch, any record/correlation-envelope/head epoch mismatch, and any sequence-local rollback to an earlier epoch. No candidate can be admitted while only an in-memory epoch or `head.open` exists.

Startup takes an exclusive non-blocking directory lease and requires exactly one valid `head`, including at sequence zero. It rejects a missing, deleted, truncated, malformed, or hard-linked head; a head rollback whose sequence/hash does not equal the complete record scan; non-regular/hard-linked published entries; unknown names; duplicate sequences; gaps; unknown schema versions; oversize records; any record/head/correlation epoch mismatch; hash-chain/head mismatch; and a pre-existing `.open` (including `head.open`). It does not guess, truncate, delete, skip, regenerate an epoch, or `unwrap_or` malformed state. Only a directory created by this startup invocation may be initialized; a pre-existing empty directory is `InvalidExistingLedger`, so deleting the head and complete record set cannot masquerade as a fresh ledger. Directory creation is allowed only at the compile-pinned path with mode `0700`; it is persistence setup, not arming or suppression provisioning.

Capacity is preflighted before the irreversible R9 claim. Any I/O error after preflight but before durable publication is a typed `PersistenceFailed`; because the claim/signing outcome may already be irreversible, the worker closes and does not retry that candidate.

### 7.1 Operator-visible closure and recovery

Ledger shutdown is not emitted as an ordinary `BridgeRejected`, `Busy`, or `Closed` candidate result. The entrypoint publishes one sticky `SimulationEntrypointStatus::LedgerClosed` value and one structured `error!` event. Its closed reason is total and distinguishable:

- `Full { ledger_epoch, next_sequence: 262144, capacity: 262144 }`;
- `PersistenceFailed { ledger_epoch, next_sequence, operation, io_kind }`;
- `InvalidExistingLedger { ledger_epoch: Option<_>, class }`.

`operation`, `io_kind`, and `class` are bounded enums, not interpolated paths or arbitrary error strings. These exact `Full`, `PersistenceFailed`, and `InvalidExistingLedger` variants are exhaustive. The selected value is write-once and remains queryable for the process lifetime, including after the worker and sink close, so an operator can distinguish capacity exhaustion, a transient-looking write failure, and structural corruption after the original log event. The T4e sink then reports `Closed`; it never clears, renames, overwrites, or downgrades the sticky ledger reason.

S2 performs no automatic rotation, deletion, truncation, in-place repair, or retry. Recovery is an explicit owner act:

1. stop the node and preserve the complete pinned directory;
2. inspect capacity/disk/filesystem health and archive the directory under an owner-chosen immutable location without modifying its records;
3. for `InvalidExistingLedger`, retain the invalid directory for forensic review rather than repairing it;
4. move the entire old directory away atomically, leave the compile-pinned path absent so startup creates and durably initializes it, and restart the explicitly built arm-sim node;
5. verify the startup status reports a new random `ledger_epoch` and sequence zero before resuming the campaign.

The new epoch is generated from OS randomness when startup creates the absent ledger directory, durably stored in the initialized head before record admission, and included in every record and correlation envelope. The operator must verify that it differs from the preserved directory's epoch as well as verifying sequence zero; equality is treated as rollback/reuse and remains stopped for investigation. This prevents cross-rotation sequence confusion without claiming that the unsigned ledger authenticates itself against an adversary who rewrites the directory and operator evidence together. If the owner does not perform this act, the correct recovery is “remain stopped and escalate”; there is no hidden reset surface. S2 documents this procedure but does not execute it, add a reset command, or treat restart alone as recovery.

## 8. Typed terminal results

The worker classifies every accepted candidate exactly once:

- `Persisted { correlation: SimulationCorrelationEnvelopeV1 }`;
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
- `H0 GREEN`: initialized-empty and populated ledgers reopen only when the required 72-byte head exactly matches epoch, next sequence, and latest record hash;
- `H1 RED`: delete/truncate the head, tamper any head field, or roll the head back to an earlier valid sequence/hash;
- `L0 GREEN`: each exact sticky `LedgerClosed` reason (`Full`, `PersistenceFailed`, and `InvalidExistingLedger`) remains independently queryable after sink closure;
- `L1 RED`: remove, rename, merge, overwrite, or clear any sticky closed reason;
- `X0 GREEN`: record, head, and correlation-envelope schemas all require one identical non-zero 32-byte ledger epoch and preserve it across restart;
- `X1 RED`: remove the epoch from any one schema or make it optional;
- `X2 RED`: mismatch the record, head, and correlation-envelope epochs;
- `X3 RED`: roll an epoch back in a record, head, or correlation envelope after a new generation is established;
- `U0 GREEN`: aggregate `ProductionInstallationDeferred`, the named follow-up, its `deferred_production` sink returning `T4eHandoffError::Rejected`, and successful node startup remain explicit;
- `U1 RED`: remove or change the aggregate deferral reason, constructor, or follow-up installation name;
- `U2 RED`: change `Rejected` to silent acceptance or candidate retention.

Every mutant first asserts its patch changed the input. At least `S0`, `E0`, `Q0`, `P0`, `H0`, `L0`, `X0`, and `U0` stay GREEN.

Existing seals are not weakened. Expected explicit amendments are limited to:

1. CLI/node feature allowlists gain the exact `arm-sim` rung and live features compose it;
2. the T4e handoff surface gains the reviewed S2 installer/entrypoint facade;
3. the economics/validated-transaction allowlists gain the immutable receipt and correlation accessors;
4. the arm root allowlist gains only the high-level simulation entrypoint/status/store record types required by CLI wiring;
5. arm backend, live permit, reqwest, socket-site, T4b, linker, and exact PR#48/#50 default-closure assertions remain intact.

Each amendment is listed in the implementation PR body with before/after counts.

## 10. Implementation placement

Meaningful logic remains outside `lib.rs` and binary glue:

- PR #52 submit crate: arm simulation worker/library entrypoint, typed unavailable status/sink, correlation projection, bounded store, economics receipt propagation, seals/tests;
- **Production T4e Simulation Installation + Settled-Loss Authority**: CLI trader node-binding for the complete prerequisite conjunction, shared bridge installation, production worker ownership, and graceful shutdown;
- CLI/node manifests: exact feature forwarding only;
- node binary: no meaningful logic.

No generic persistence framework or network sink abstraction is introduced. PR #52 makes no production `send_gated` caller reachable.

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

PR #52/S2 is complete when it supplies the default-off simulation-only rung, bounded non-blocking worker/library entrypoint, typed Rejected-only unavailable sink, and bounded joinable EV-recomputable durable simulation store while retaining broadcastability zero. It does not install the first production `send_gated` caller and does not claim production `Busy`, `Closed`, or `Ready`. Production reachability is complete only in **Production T4e Simulation Installation + Settled-Loss Authority**, after every prerequisite in §3.2 is installed as one identity-matched conjunction, admission is reserved before claim/signing, and the existing unified witness precedes durable simulated submission.
