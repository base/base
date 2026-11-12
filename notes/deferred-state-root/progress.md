# Deferred state root implementation checkpoint

This note records the baseline and validator-publication feasibility gate for the approved deferred state root design. This is an incomplete, test-only WIP checkpoint, not an implementation of the fork and not correctness or performance acceptance.

## Branch and scope

- Branch: `poc/deferred-state-root-on-6268`.
- Starting HEAD: `4d4a6fd3f81e6817f00f3c80c5a160a5b7cdab3e`.
- Source baseline: `432d627174bef54e4b39d7ca9b3e4938300c256e`.
- Approved `docs/design/deferred-state-root.md` remains unchanged.
- No reth patches, merges, rebases, pushes, PRs, cloud sessions, deployment or benchmarks.
- No production validator, schedule, builder, runner or service behavior changed.
- New direct test dependencies reuse existing locked packages. Reth remains official v2.10.1 `58ebce14ae63dab040beae301e1578cd0dc55e8a`; EVM2 pins are unchanged. `reth-storage-overlay` is newly exposed in workspace dependencies at the same official pin.

## Immutable baseline evidence

Files are in ignored `target/deferred-root-gate1/`; no earlier experiment files were overwritten.

- `baseline-tree.txt`: exact baseline Git tree identity.
- `baseline-fixtures.tar`: Git archive from the exact baseline containing Cargo manifests/lock, reth pin manifest, existing node genesis fixtures and native builder benchmark fixtures.
- Archive SHA-256: `294c65f57f1c46c6b2da002e6a7bc2342b63affc86de6bc71322965653ae5a2e`.
- Existing node genesis fixture SHA-256: `0c7a8b2b057891ac8b6f4243c609ca868b30f0ea6c11c220e6bcbafdfb227425`.
- `immutable-baseline/base_execution_payload_builder-63863eaa74e4eb84`: copied baseline test binary. SHA-256 `cd1b45d016ae4485cb2763dc000bb1d44dc5957de255325bb871009b9df3048d`.
- Binary was produced before any source edits. Its digest and copy were recorded after adding only node test plumbing, before any payload source edits. There are still no payload source edits.
- This archive/binary is NOT a complete payload/header/receipts/state/trie/status/output/proof golden. That required baseline fixture has not yet been generated.

## Actual wrapper gate

`crates/execution/node/tests/deferred_state_root_gate.rs` is an integration test prototype using the real pinned `BasicEngineValidator`, `BaseBeaconConsensus`, `BaseEvmConfig`, `BaseEngineValidator`, a custom `StateRootStrategy`/job, an `EngineValidator`/`WaitForCaches` wrapper, and `EngineApiTreeHandler::spawn_new` with real newPayload responses.

The provider is the existing scripted `MockEthProvider`, explicitly used only to isolate publication from MPT correctness. Its configured/default scalar is not an independent real MPT computation. No deferred commitment assertion is certified by this gate.

The successful gate checks:

- A custom job receives finalized execution/hashed state and privately stages sorted inputs.
- Successful inner validation is required before publication.
- Ready replacement retains recovered-block and execution-output Arc identity, BAL Arc identity, timing allocation identity, exact hash and exact sorted trie inputs.
- A deliberately abandoned inner pending handle is replaced without reading it.
- The real engine returns VALID with exactly the fixture block hash.
- The test runtime owns the stock redundant sorting task. Both success and fault tests explicitly terminate the engine thread and shut down the runtime; the Base producer thread is joined.

Fault gates inject publication sender drop, producer panic, and timeout after actual inner validation. Each returns an operational engine error, exposes zero successful wrapper outputs and bounds the wait. They exercise publication-channel failure, not a production manager or production root-worker failure. The test explicitly terminates the engine after the response. It does NOT prove the future node automatically stops or reports unhealthy on operational failure. Production stop/health policy and duplicate-sort metrics remain required.

BAL is present as raw payload extras and preserved on the executed result. The fixture is pre-Amsterdam and does not assert a BAL header commitment. Initial attempts to add that header field failed the existing Base payload round trip's block-hash check. Removing the field preserved the requested BAL-present publication exercise without claiming Amsterdam payload-format support.

## Commands and exact results

The Justfile was inspected. `just test-ci` hardcodes workspace scope and has no package parameters. Focused commands below use its stack size, nextest CI profile, locked resolution, all-features and Cargo CI profile, replacing workspace scope with an affected package/test target. No fmt, clippy or zepter ran.

1. `just check-reth-pin`: PASS, initially 68 existing git crates. Logs `check-reth-pin1.log`; later checks `check-reth-pin2.log` through `check-reth-pin4.log` PASS with 69 git crates after exposing the existing overlay dependency.
2. `just pin-reth-test`: PASS, 22 passed, 0 failed. Logs `pin-reth-test1.log` and `pin-reth-test2.log`.
3. `RUST_MIN_STACK=8388608 cargo nextest run -P ci --locked -p base-execution-payload-builder --all-features --cargo-profile ci`: PASS, 161 passed, 0 failed, 0 skipped. `baseline-payload1.log`. This predates source edits. The earlier workspace SQLx problem did not block this focused package.
4. `RUST_MIN_STACK=8388608 cargo nextest run -P ci --locked -p base-node-core --all-features --cargo-profile ci --test deferred_state_root_gate`: initial `wrapper-gate1.log` and `wrapper-gate2.log` were test-code compile failures, not acceptance red evidence. Fixed generic arguments, trait import, mock constructor, channel type and payload type inference.
5. Same gate command, `wrapper-gate3.log`: test-fixture setup failed before validation because the chain builder lacked a chain. Nextest reported 0 passed, 3 failed, 0 skipped, 1/4 not run after cancellation. Not acceptance red evidence.
6. Gate command with `--retries 0 --no-fail-fast`, `wrapper-gate4.log`: 0 passed, 4 failed, 0 skipped. Real engine block-hash mismatch from a BAL header field the existing Base payload conversion did not round-trip. Fixture defect, not deferred-root red evidence.
7. Same gate command with `--retries 0 --no-fail-fast`, `wrapper-gate5.log`: 4 passed, 0 failed, 0 skipped.
8. Final gate command after explicit engine termination was added to fault paths, `wrapper-gate6.log`: 4 passed, 0 failed, 0 skipped. No warnings in this final run.
9. `bash etc/scripts/ci/check-crate-deps.sh`: PASS, logs `check-crate-deps1.log` and `check-crate-deps2.log`.
10. `git diff --check`: PASS. Cargo metadata resolved offline; lockfile changes only add the node's three direct test dependencies, with no package/source/version repin.

Final passing test counts are 161 existing payload tests, 4 new integration gates and 22 pin-helper tests. No entire-workspace test/build was attempted. Other affected functional suites are unrun because their production implementation does not exist yet.

## Must-hold acceptance status

| Criterion | Status and evidence |
|---|---|
| c1 flag_off_parity | NOT SATISFIED. Existing 161-test baseline passes, but exact full goldens/differential output, zero constructor/job counters, hook/prewarm matrix and candidate replay remain absent. |
| c2 independent parent check before echo | NOT SATISFIED. No production deferred job or independent parent-root fixture exists. No genuine initial assertion failure captured. |
| c3 wrong deferred root INVALID with exact latestValidHash | NOT SATISFIED. Real engine plumbing exists, but the scripted provider publication tests do not test this consensus contract. No genuine initial assertion failure captured. |
| c4 activation F/F+1 distinct independent roots | NOT SATISFIED. No fork schedule or chained independent-root fixture exists. No genuine initial assertion failure captured. |
| Wrapper integration prerequisite | Publication feasibility passes the four real-engine tests above, with the scripted-provider, pre-Amsterdam and test-prototype limitations stated. Not a production safety claim. |

## Remaining work in order

1. Generate the exact immutable baseline goldens/differential fixtures and write/run all required must-hold semantic tests FIRST. Record genuine c2/c3/c4 assertion failures, not these fixture/compile failures.
2. Implement Never-default custom-only fork scheduling, built-in-network rejection and serialized-default parity.
3. Implement the bounded hash/parent/number manager with separate builder/validator provenance, real stable providers/overlays, retention/admission/deadlines/cancellation and operational stop/health propagation.
4. Implement the actual deferred strategy/job and production wrapper, with private staging until all inner checks pass, parent-root equality before any echo, actual current updates and ready publication; instrument/count redundant sorting.
5. Detach builder current-root work after authoritative finalization, preserve commit Yes/No/system transitions and disable trusted insertion only in the active/re-execution cohorts.
6. Add disposable-state startup markers before reth recovery and fail-closed sync/reorg/proof/output/L1/service guards, including debug-skip/witness rejection.
7. Add matched O/S/R/D controls, accounting and the remaining acceptance/regression/fault/reorg tests. No performance claim or benchmark run is authorized.

Do not push or open a PR from this checkpoint. The implementation is incomplete. Formatting and lint checks remain for the user, as requested.

## Continuation: real-state baseline and active validator adapter

Evidence directory `target/deferred-root-gate2/` is new; gate1 data was preserved.

- Real disposable MDBX/static-file/RocksDB fixtures execute deterministic signed value transfers with BaseEvmConfig and calculate actual MPT roots/updates. No provider scalar is scripted.
- `semantic-baseline7.log`: c1 passes, c2/c3/c4 fail on semantic assertions. c2 stock rejects the independently computed parent root; c3 stock accepts the wrong deferred same-block root; c4 stock emits current root rather than parent. c4 also computes a second transfer and checks all three actual roots differ before its commitment assertions. This assembly fixture does not yet exercise the native payload builder.
- `semantic-baseline1.log` through `semantic-baseline4.log` are compile setup failures, not red evidence. `semantic-baseline5.log`/`6.log` also contain genuine assertions but the first Debug-based golden was nondeterministic. It was replaced with deterministic sorted materialization before production edits.
- Immutable deterministic golden `baseline-golden2.txt`: SHA256 `777fc96a5a88224f2eb92aec4237b79d53bb168fc5b8e75d32525598f1dc9f16`, Keccak `12429246b6b6753619b2b0980eec53f7d37b90a21bd0b3a0bdd6ad31e4822e3b`. Covers block/payload, receipts, hashed state, sorted trie nodes, exact Engine status. It does not cover output/proof services, prewarm/hook matrices or native builder parity.
- `semantic-golden-replay1.log`: exact immutable golden replay passes before production changes.
- New production manager and validator adapter are NOT installed on node launch paths yet. Adapter independently resolves actual parent from public OverlayStateProviderFactory and computes current root/updates/sorted inputs with bounded serial work. Wrapper publishes only after inner validation success and constructs ExecutedBlock::new preserving BAL and existing timing allocation without reading stock pending trie data. Stock sorting remains runtime-owned and is not cancelled by this wrapper.
- `manager1.log`: 3 new manager lifecycle tests pass, 161 pre-existing payload tests filtered out.
- `strategy-semantic2.log`: c1 immutable golden replay passes through off wrapper with zero manager construction; c2 correct deferred parent passes real engine validation; c3 wrong deferred commitment returns INVALID with exact parent latestValidHash. c4 remains intentionally failing pending builder work. No no-patch blocker was found.
- Remaining scope is still substantial: complete manager abandonment/admission/retention tests and builder provenance waiting, production schedule/wiring, owned detached builder snapshots, native builder activation test, pre-launch/storage/sync/service guards, controls/metrics and regression/fault/reorg suites. No benchmark/deployment or external interaction performed.

## Continuation: native builder, custom schedule and guarded runner

The earlier checkpoint's “no production changes” statement describes that earlier commit only; production implementation is now in progress.

- Native builder uses an owned parent StateProviderBox captured by build_payload, finalized authoritative execution output, actual parent MPT fallback, and detached current-root calculation. Active payloads expose no trusted executed block. Yes/No admission callbacks remain unchanged; mixed speculative/system regression coverage remains outstanding.
- New configuration-only DeferredStateRoot schedule is Never by default and excluded from contract/runtime signaling. Constructors and CLI reject mainnet/Sepolia, genesis activation, non-timestamp activation, or missing Bedrock/Regolith support.
- Engine builder delegates stock construction directly (preserving hook/prewarm setup), installs captured provider/overlay strategy only for configured experiments, and owns critical health/shutdown drain monitoring.
- Guarded runner admission precedes reth launch. Durable external marker forbids all reopen attempts. Immutable pre-fork manifests authenticate store bytes. Read-only checkpoint/tip/root checks precede stock consistency recovery; staged/backfill/deep-reorg requests terminate before stage execution. Production extension/L1/proof/output services fail closed. Snapshot consistency and recovery rejection coverage is not yet exhaustive.
- `native-semantic3.log`: c1-c4 all PASS. c4 builds F/F+1 through the production native builder, independently validates each against shared builder provenance, persists F with its delayed header, then validates F+1 using actual persisted MPT state. Exact current roots/updates are checked after VALID. Pairwise distinct real roots remain required. Immutable c1 digest is now asserted unconditionally.
- `builder2.log`: payload suite 164 PASS before additional manager tests. `manager3.log`: seven manager lifecycle tests PASS.
- `runner-guard4.log`: runner all-features library cargo check PASS. `runner-guard-tests1.log`: five guard tests PASS, eleven filtered out. Earlier runner compile failures are setup, not red evidence.
- `cohorts1.log`: 318 tests PASS across base-node-core, base-execution-chainspec, base-common-genesis and base-common-chains, including original four wrapper gates and real c1-c4. This predates the latest serial control tests/minimal CLI selection.
- O/S/R/D controls and metrics are being completed; no benchmark/deployment or performance claim. No reth source/pin change.
