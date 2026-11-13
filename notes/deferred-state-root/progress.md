# Deferred state root implementation progress

This chronological log records implementation and verification of the approved deferred state root design. The first sections describe the earlier test-only checkpoint; they are historical, not the current implementation status. The latest continuation and remaining acceptance gaps are at the end. No performance acceptance is claimed.

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

## Continuation: matched controls and safety/accounting checks

- Added explicit O/S/R/D selection (`--rollup.root-experiment-cohort`) and queue/retention/deadline inputs. S/R use the same serial owned-provider worker as D; S retains trusted execution, R/D reject bypass and expose no trusted object. All controlled cohorts use the extension-free disposable runner and the same proof/sync/L1 restrictions. Ordinary default options are untouched. Native custom payload service propagation was added; its SQLx-dependent package cannot be compiled on this host (see below).
- Added worker admission/root/lag/wait/sort wall-time counters, independent builder/validator execution and redundant stock sort counts, separate immutable-seal and ready-validator-output events. These are NOT Engine VALID/canonical acceptance measurements. CPU/perf/RSS/DB/persistence workload accounting is still a future authorized benchmark harness responsibility; no throughput claim is made.
- `cohorts2.log`: six semantic/control tests PASS (before bounded S/R worker revision).
- `regression1.log`: 450 tests PASS across five affected library packages (payload, node, runner, chainspec, consensus engine). Includes seven manager tests, new output/config guards and five startup guards. Later changes require rerun.
- `semantics-matrix1.log`: nine real-state tests PASS. Includes exact Never/pre-F stock-vs-wrapper statuses and invalid-hook counts with prewarming enabled/disabled; malformed deferred root hook sees exactly computed parent/rejection-only updates; barrier verifies native seal returns while current root is blocked and Engine response remains absent after validator root admission.
- `health-guards2.log`: eleven focused tests PASS (442 filtered); nextest labels snap guard leaky. Actual provider-worker panic automatically reaches reth Runtime's fatal-task handle, drains/joins the worker first, and publishes zero ready entries. Graceful shutdown joins the worker. Snap serving is now explicitly rejected, not just snap backfill.
- `runner-engine1.log`: two actual full-runner integration tests PASS. Ordinary and configured deferred custom chains perform FCU->native getPayload->independent newPayload->canonical FCU; D checks parent commitment, public proof RPC rejection, and graceful owned-work drain. This is isolated EL replay, NOT Base CL safe derivation/L1 support.
- `semantics-reorg3.log`: nine tests PASS, one same-height/activation memory-reorg test reaches all semantic assertions but times out during test teardown because its mock persistence receiver never services termination's forced flush. This is a fixture lifecycle defect, not evidence of a root-consensus failure. The fixture is being corrected to return to genesis before termination; production persistence coverage exists in the real-runner test.
- `cli-check1.log`: scoped execution CLI cargo check cannot load `libsqlx_macros-691b2016a76e2857.dylib`: macOS `mis-aligned LINKEDIT string pool, fileOffset=0x0066F52C`. No linker workaround or full workspace retry. Runner scoped checks/tests work.
- Disk reached 609 MiB free. With explicit user approval relayed by parent, verified no cargo/rustc/nextest was running and removed ONLY this worktree `target/ci/incremental` reproducible cache (3.3 GiB). All numbered evidence, immutable archives/binaries and experiments were preserved. No broad clean.
- `check-reth-pin1.log`: PASS, 69 existing official v2.10.1 git crates, no reth patch/pin drift. `pin-reth-test1.log`: 22 PASS. `check-crate-deps1.log`: PASS.

## Final continuation: real edge cases, detached deadlines and read-only admission

Continuation began at `246d5a420b40dd11479e59081c8d94b2f5023155`, following the historical checkpoint above. Local implementation milestones are `82ff31923`, `7932190c0`, and `1778f1642`; the final safety/test milestone follows this entry. Approved design and reth source/pins remain unchanged.

Implemented beyond the prior continuation:

- Real native builder/Engine tests now cover a pool CommitChanges::Yes transfer, an executed over-budget SSTORE rejected with No, and a forced deposit together. Exact final body, ordered commit/invalidation callbacks, independently calculated current MPT root, actual trie updates and independent validator certification are asserted. Borrowed speculative views never enter the detached job.
- Additional real fixtures cover reverting storage writes, legacy SELFDESTRUCT, and three blocks deploying/deleting/recreating exactly one CREATE2 address. Each block independently calculates actual roots/updates, validates against builder provenance and persists a delayed header. Roots are pairwise distinct.
- A cold bounded manager test explicitly misses the accepted parent entry after F was persisted with a delayed header. Both the builder and independent validator recover actual DB MPT state; copying the delayed header would fail its assertions.
- Memory forks test now also reseals a wrong deferred descendant, rejects that block and its child with exact latestValidHash at the valid branch parent, rejects FCU to the invalid head, and verifies canonical head did not change. The fixture uses `with_unwind_canonical_header(true)` and an unserved persistence handle; full real persistence/Engine plumbing is tested separately by the runner tests.
- Full ordinary/deferred runner tests reject malformed blockHash and resealed wrong same-block withdrawalsRoot, then accept the valid payload, accept its exact duplicate and canonicalize through FCU. Ordinary public/auth proof responses match exact account/storage proof bytes; D fails closed.
- Stock malformed-blockHash behavior reports the exact known parent, not None. `final-affected4.log` and `final-semantics3.log` expose an incorrect new test expectation for both ordinary and D. The expectation was corrected to the observed unchanged pinned behavior; production behavior was not changed to satisfy it. These are not must-hold baseline red evidence.
- Detached builder operations now retain admission deadlines independently of validator waiters. The production runtime monitor checks them while the serial provider is blocked. A late completion checks health before publication, so a timed-out job cannot publish even if no validator ever arrived. An automatic timeout test reaches the runtime fatal channel without manual terminate; the unpreemptible provider remains owned until explicitly released and joined in the test.
- Local provider errors are explicitly tested against pinned `ensure_validation_error`: exactly engine-fatal Provider handling, never consensus INVALID ancestry. Panic/failure health handling and graceful worker drain remain tested. Runner launch-error paths also drain already-started provider work.
- Read-only startup guard now requires unpruned snapshots, exact durable Finish checkpoint for non-genesis tips, matching static frontiers, safe/finalized metadata within the tip, no stale genesis RocksDB transaction lookup, and consistent changeset sidecars. It reuses pinned static file checks through an explicitly read-only provider; it never invokes healing through the writable factory. A real interrupted static append is rejected without truncating any bytes. Configured pruning, snap serving and both forward/deep-unwind backfill requests are rejected before dangerous stages.
- Configured D rejects production CL L1 service startup before any transport, legacy proof-config projection and stateless proof execution even before activation. Upgrade-signal startup guard is centralized so both CLI run and launch paths reject before an L1 call.
- Accounting distinguishes builder/validator execution attempts, actual current roots by independent owner, root versus sorting wall time, duplicate stock sorts (including wrapper rejection), S trusted insertions, immutable seal and ready validator output. Engine VALID/canonical acceptance and CPU/RSS/persistence accounting remain external harness measurements, not falsely inferred events.

Latest completed checks (numbered evidence remains in `target/deferred-root-gate2/`):

| Evidence | Result | Scope |
|---|---|---|
| `final-affected5.log` | 480 PASS, 0 skipped | Five affected library packages plus real semantic tests, original four publication gates and ordinary/deferred full-runner tests; latest production code. |
| `final-correctness1.log` | 39 PASS, 438 filtered, 1 leaky | Latest semantic/manager/health/admission/runner tests, including the additional cold persisted-parent test. Overlaps the 480-test run; do not add raw counts as unique tests. |
| `proof-common2.log` | 246 PASS, 0 skipped, 1 leaky | Common chain/genesis, proof primitives and stateless proof executor; includes new fail-closed proof tests. |
| `service-guard-check1.log` | PASS | All-features base-consensus-node cargo check. |
| `service-guard-test1.log` | 1 PASS, 427 filtered | Actual production service guard before invalid IPC/L1 transport. |
| `rpc-regression1.log` | 101 PASS, 1 skipped | Execution RPC and common RPC types; the existing ignored WS test requires localhost:8546. Five other package binaries were excluded by the package filter. |
| `check-reth-pin2.log` | PASS | 69 official v2.10.1 git crates. |
| `pin-reth-test2.log` | 22 PASS | Pin-helper tests. |
| `check-crate-deps2.log` | PASS | Repository dependency placement. |

All Rust runs use `RUST_MIN_STACK=8388608`, `--locked --all-features --cargo-profile ci`, nextest `-P ci --retries 0 --no-fail-fast`, and explicit package/test scope. The Justfile's test-ci recipe is workspace-wide, so these are documented scoped equivalents. No full workspace build, fmt, Clippy or zepter was run. One aggregate shell call timed out while reaching git diff --check under concurrent tests; pin/dependency checks had already passed. A later standalone diff check passed.

## Prior checkpoint acceptance and remaining limits

- c1: immutable pre-production exact block/payload/receipt/hashed-state/trie/status digest remains PASS, zero-manager Never/pre-F assertions PASS, and stock/wrapper compatible-hook/prewarm enabled/disabled matrix PASS. Native pre-F exact executed state/trie parity also PASS. **The full requested immutable baseline output/proof RPC golden and populated txpool-cache prewarming matrix are not complete.** The new ordinary proof transport comparison is supplementary, not a fabricated pre-production golden.
- c2/c3/c4: genuine semantic red evidence is `semantic-baseline7.log` before production changes (c1 PASS, c2-c4 FAIL). Native selectable legacy wiring later independently reproduces c1 PASS/c2-c4 FAIL in `native-legacy-red1.log`; that later run is not mislabeled pre-production. Current real builder/validator/root/ancestry/activation tests PASS.
- No no-patch feasibility blocker was found. Current roots, actual updates and fully sorted materialization complete before successful validator publication; builder sealing can return while its current root remains blocked. The prototype scripted gate remains publication-only evidence, not MPT proof.
- CLI compilation remains blocked by the recorded macOS SQLx dylib LINKEDIT failure in `cli-check1.log`; no known-bad linker workaround or full build retry. The CLI and native custom builder-service source changes are not fully package-compiled on this host. The consensus service guard was separately checked/tested successfully.
- Immutable consistent non-genesis snapshot copy/replay through the full launch path, exhaustive interruption at each MDBX/static/RocksDB boundary, and populated txpool-prewarming behavior still need broader supported-host/system coverage. Static corruption rejection and fresh-genesis runner tests do not substitute for that matrix.
- General active restart, staged/snap/deep-reorg recovery, proof/output/L1 derivation and settlement are deliberately unsupported and fail closed. No production bridge/proof integration, benchmark, base-metal run, deployment, push or PR was performed.
- `notes/deferred-state-root/base-metal-inputs.md` records O/S/R/D controls, immutable artifacts and remaining accounting inputs. No standalone immutable baseline node binary or fully built candidate CLI is claimed: the preserved baseline executable is a payload test binary.
- Full approved acceptance is **not claimed** while these validation/artifact gaps remain. Keep the branch local; do not open a PR or benchmark it as accepted. User handles formatting and lint checks.

## Safety continuation from d305023

This continuation closes two independently verified safety defects and adds immutable baseline output/proof and populated-prewarm evidence. The prior checkpoint's gaps and claims above describe that checkpoint, not the corrected implementation. New numbered evidence is in `target/deferred-root-safety1/`; prior evidence was not overwritten.

### Genuine defect failures and fixes

- `retention-red1.log` is a compile/setup failure; `retention-red2.log` caught a missing fixture advance and is not defect evidence. `retention-red3.log` is the genuine production failure: native S builds and successful trusted insertion pass twice with retention cap 2, then the third real native build fails with `ready retention full with unverified builder results`. The fixed test builds/inserts five consecutive real blocks with cap 2 and requires an empty healthy ready/FIFO registry afterward.
- Successful controlled trusted insertion now removes only its own Builder provenance after the inner hook returns success. It never publishes Validator provenance. The injected inner-insertion failure test retains exactly the original Builder and FIFO entries and no Validator entry. A separate capacity test proves unmatched R/D Builder roots cannot be evicted to make room.
- `snapshot-red1.log` is a compile/setup failure. `snapshot-red2.log` exposed MDBX mutation from the fixture's own writable-open boundary and is not the clean runner defect reproduction. `snapshot-red3.log` is the genuine full-launch assertion failure: an authenticated uncertain snapshot reaches rejection, but writable reth RocksDB opening has already replaced logs, MANIFEST, OPTIONS and CURRENT bytes/filenames. MDBX was supplied read-only for that reproduction.
- Snapshot semantics now run before writable reth static/RocksDB opening. Runner admission uses only RO MDBX provider transactions on the already-owned environment and explicit read-only static/Rocks handles. The later recovery guard remains. Tests exercise both an authenticated active tip and inconsistent safe metadata, on both the full-runner and before-init caller boundaries. Exact immutable filename/digest maps for MDBX/static/Rocks must remain identical; OS lock filenames must also remain identical. As in the manifest format, mutable OS lock contents are excluded, not falsely described as immutable.
- The observed CLI caller boundary is handled before `init_db`: shared `DeferredStateRootPrepared::prepare_database` opens standalone read-only MDBX/static/Rocks handles, closes them after validation, and only then initializes writable MDBX. Its private single-use handoff binds chain, canonical datadir, schedule/cohort/limits/arguments, manifest and marker bytes. The runner reruns configuration and semantic guards but does not reauthenticate bytes intentionally changed by admitted initialization. Private token fields are intentional to prevent a caller from forging the bypass.
- Typed embedded CLI launch carries the sealed handoff. Generic CLI execution uses the existing `HasRollupArgs` trait and rejects D/S/R, snapshot and disposable inputs before upstream `NodeCommand::execute`; it directs experiments to the admitted API. Default ordinary startup is unchanged. Unmarked MDBX-only state must have no chain-data entries; executable VersionHistory is the only allowed populated table for a fresh initialized DB.

### Immutable output/proof and populated txpool evidence

- Exact baseline `432d627174bef54e4b39d7ca9b3e4938300c256e` was archived inside ignored `target/deferred-root-safety1/reference1/`. `reference-source-attestation1.json` verifies all 2,577 baseline blobs are unchanged, tree `04c3367bb1bafbe3414463e81c7cc525220e4a0d`. Only a test-only fixture and subsequently its golden asset were added; no baseline production bytes were changed.
- Identical output/proof generator fixtures ran on the candidate (`rpc-fixture-setup2.log`) and the archived baseline (`reference-rpc1.log`). The latter actually generated `baseline-rpc-golden1.json` using `BASE_RPC_GOLDEN_WRITE` with create-new semantics. This is not a post-change golden relabeled as baseline. The fixture drives actual full-runner FCU/native payload/newPayload/canonical FCU, public/auth message-passer account/slot proof RPC, and the production output query. For the isolated output query it explicitly treats the persisted block as its rollup anchor; it does not claim L1 derivation/deposit support.
- Golden SHA256: `e5d0c442004dd48f17384f7862441dd89d1b948909439f81b24f90d8668b1d0a`. It includes exact proof bytes, 128-byte output-v0 preimage, output hash and block hash. The candidate fixture now unconditionally compares the tracked `tests/assets/deferred-root-baseline-rpc1.json`; final replay is part of the affected suite.
- Preserved immutable baseline **test executable**, not standalone node: `immutable-reference1/rpc_baseline_parity-19e865c7f4beffda`, SHA256 `df01311afc95d97decca0f19efafe80abe07e1d4b11b7781c46fcdd6c6a05a89`. Generator fixture SHA256 `0008c616c62f3379eac365108c6150c74953e7f7bc550b29adb313610b04275b`. The later unconditional golden assertion and helper doc comments are recorded separately in final candidate hashes, not attributed to the generator binary.
- The populated-prewarm matrix installs a real signed-transfer pool source and waits for the pinned worker's actual structured snapshot publication, not an assumed delay. It asserts exactly seven cached accounts, two storage slots and zero bytecodes, exactly one publication, exact stock/wrapper status/latestValidHash, exact hook counts, and zero manager/job construction for Never/pre-F with block prewarming enabled/disabled and valid/invalid roots. This covers a populated snapshot rather than only toggling prewarming on an empty pool. It does not measure cache hit-rate performance.
- Archive and candidate local packages sharing a target directory exposed stale cross-root Cargo artifacts in runner-only scopes (`runner-latest1.log`, `runner-latest2.log`). These are compile/cache failures, not safety red evidence. No cache/evidence was deleted. Candidate source mtimes were refreshed and affected payload/RPC/node/runner packages explicitly rooted together for the complete candidate suite. Future baseline builds should use a separate target directory.

### Verification before local implementation commit

All Rust runs retain the scoped Justfile test-ci conventions documented above. Exact command forms are in `target/deferred-root-safety1/evidence-commands1.txt`. Package roots for the final scope are payload-builder, execution-rpc, node-core, node-runner, chainspec and consensus-engine; targets are their libraries plus `deferred_state_root_gate`, `deferred_state_root_semantics`, `in_memory_engine` and `rpc_baseline_parity`.

| Evidence | Exact result | Interpretation |
|---|---|---|
| `reference-rpc1.log` | 1 PASS, 0 failed, 0 skipped | Actual immutable baseline output/proof generator. |
| `parity-green3.log` | 2 PASS, 1 failed, 105 filtered | Output/proof candidate comparison and injected insertion failure passed; populated snapshot expected-count setup was corrected from assumed 3/0/0 to observed OP predeploy-inclusive 7/2/0. |
| `prewarm-green4.log` | 1 PASS, 0 failed, 17 filtered | Entire populated stock/wrapper matrix passes as one test. |
| `regression1.log` | 488 PASS, 1 failed, 0 skipped, 1 leaky | Fresh-DB guard initially rejected init_db's VersionHistory row; fixed narrowly to permit only executable-version metadata. |
| `regression2.log` | 515 PASS, 1 failed, 1 skipped | Only new optional non-genesis snapshot fixture failed from retained in-memory RPC DB ownership. |
| `regression3.log` | 514 PASS, 2 failed, 1 skipped | Snapshot fixture needed a fresh immutable store copy; one unchanged randomized consensus-attributes test also failed. |
| `regression4.log` | 515 PASS, 1 failed, 1 skipped | All candidate safety/parity/real-MPT/snapshot-copy launch checks passed; ONLY unchanged randomized transaction-content test failed. Not a full-suite pass. |
| `snapshot-copy1.log` | 2 PASS, 0 failed, 492 filtered | Immutable non-genesis snapshot launch and explicit targeted rerun of the randomized test passed. |
| `snapshot-replay2.log` | 1 PASS, 0 failed, 493 filtered | Latest test additionally executes FCU/native getPayload/newPayload/canonical FCU from restored snapshot height 1 to height 2, checking exact ancestry, VALID hash and canonical height/hash. |
| `check-reth-pin1.log` | PASS, 69 official git crates | No reth patch or pin drift. |
| `pin-reth-test1.log` | 22 PASS | Pin-helper tests. |
| `crate-deps1.log`, `diff-check1.log` | PASS | Dependency placement and whitespace. |

These counts overlap; do not sum them as unique passing tests. The one ignored RPC test requires a localhost WebSocket node. The randomized failure is `attributes::tests::test_attributes_mismatch_check_transaction_content`, using `Transaction::arbitrary_take_rest`. `random-test-source-proof1.txt` records identical current/baseline file SHA256 `663a9325b8a0c42fdecfdec9e93e863759059f73fa054702d55873fe7b5dd3e2`. That unrelated test was not modified, filtered out of full runs, or silently retried. Its targeted pass does not turn a failed full run into a full-suite pass.

Current acceptance evidence closes the two safety defects, the immutable output/proof golden gap, the populated txpool snapshot matrix gap and real non-genesis immutable snapshot copy/replay. c2/c3/c4 real MPT/ancestry/activation tests remain green. Exhaustive cross-store crash interruption, general active restart, staged/snap/deep-reorg recovery, external CL/L1/proof/settlement support and performance measurements remain deliberately unsupported or future-system scope.

The latest CLI source call-site changes and custom builder-service graph are still NOT fully package-compiled on this host because of the already recorded macOS SQLx LINKEDIT problem. No repeat long SQLx attempt, full workspace build, fmt, Clippy or zepter was run. Shared admission logic and actual runner graph are compiled/tested. A supported-host CLI build and parent independent review remain required before any PR or runnable standalone benchmark claim. No push, PR, external interaction, benchmark or deployment occurred.

### Final committed-code validation

Implementation commit: `e844d7aeb` (`fix(execution): admit snapshots read-only and retire trusted roots`). After the final source change and this local commit, one last unfiltered scoped run was performed, with no exclusions and `--retries 0`:

- `regression5.log`: **516 passed, 0 failed, 1 skipped**, no leaky label. The one skipped pre-existing RPC WebSocket test needs a localhost node. All six selected library roots and four selected integration targets are included. The source identity is in `regression5-head1.txt`.
- This is the superset of the original five-library/gates/semantics/runner scope plus execution-RPC library tests and the eight new safety/parity cases. Earlier suite/targeted counts overlap and must not be added to 516. Baseline golden generation is the same logical fixture on the reference, not another unique candidate test.
- The latest real snapshot replay extension is included: snapshot copy at height 1, admitted read-only validation before writable initialization, actual native build and independent newPayload, canonical FCU at height 2 with exact parent/hash/height assertions.
- `check-reth-pin2.log`: PASS, 69 official v2.10.1 git crates. `pin-reth-test2.log`: **22 passed**, independent Python pin-helper tests. `crate-deps2.log` and `diff-check2.log`: PASS.
- The unchanged randomized consensus transaction-content test passed this final unfiltered run. Its prior full-run failures remain evidence of a residual fixture flake; this passing run does not erase them. No unrelated test or production consensus source was changed.

The following commit is documentation-only evidence recording. Parent independent rerun/review and a supported-host CLI/custom-builder package build remain prerequisites to PR readiness; no full-workspace or standalone-binary build, push, PR, deployment or benchmark is claimed.

## Verified fork-enumeration and offline-copy corrections

Continuation starts at `f464bed971c4a7c81be9a23d15b123f7db0c205f`. The parent's later independent `target/deferred-root-parent2/affected1.log` reported **515 passed, 1 failed, 1 skipped**, with `in_memory_engine_seals_one_block` failing at recursive copy with OS error 2. This supersedes any inference that the earlier 516-pass result established a reliable snapshot fixture. The old failure remains preserved.

- Parent compile red is `target/deferred-root-fork-enumeration1/red1.log`: two parity fixtures declared 14 forks while schedule enumeration now has 15. Both lengths are now derived. Production schedule storage, enum order, feature semantics and timestamp EVM resolution are unchanged.
- `spec-red1.log` is an actual pre-fix assertion failure, `DepositNonce at DeferredStateRoot`, false versus ordinal true. The historical ordinal test now explicitly handles the orthogonal marker; a separate regression checks **every** registered feature and permits only ParentPostStateCommitment on the isolated marker.
- Execution parity matrices exclude the unsupported isolated commitment-only EVM spec, rather than enabling unrelated OP features to satisfy parity. Deposit, fee, block, L1 parameter and native-precompile fixtures retain all actual execution forks. Transition-hook schedule helpers only target historical upgrades; their earlier-upgrade comparisons already exclude the marker. EIP-8130 parity already limits schedule entries through Everest. The handler's Ethereum-spec cap assertion remains valid for all entries and is unchanged.
- New schedule regression configures Regolith, Fjord and Beryl chains independently, then checks before/at/after commitment activation. It requires the exact unchanged EVM fork, Ethereum mapping, every unrelated scheduled and EVM feature flag, and actual multi-transaction block outcome on both engines. Commitment activation changes only its schedule feature. It does not imply Osaka/latest execution.
- One requested pre-fix snapshot retry, `snapshot-retry1.log`, passed. That does not erase the parent's intermittent failure. Pinned Runtime::test attaches to the caller's Tokio runtime; graceful shutdown waits registered graceful guards, not destruction of all canceled async tasks. Such tasks retain providers; pinned StaticFileProvider's StorageLock deletes `static_files/lock` on final drop. The old error did not record the vanished filename, so its exact path is not claimed.
- The fixture now constructs the existing small test runtime outside Tokio so it owns its executor, then explicitly shuts/joins it on a blocking thread after graceful drain and before snapshot copy. The source static writer lock must be absent. Exact immutable source digests are captured before copying, compared with the destination, and rechecked on the source afterward. Copy errors include both paths. No missing files are ignored, lock-copy exemptions introduced, immutable hashes weakened, or arbitrary sleeps added.

Evidence in new `target/deferred-root-fork-fixes1/`; exact commands in `evidence-commands1.txt`. All nextest runs use RUST_MIN_STACK=8388608, all features, locked dependencies, ci cargo/nextest profiles, zero retries and no fail-fast, matching scoped `just test-ci` conventions.

| Evidence | Result | Scope |
|---|---|---|
| `spec-red1.log` | 0 passed, 1 failed, 189 filtered | Genuine assertion before correction. |
| `snapshot-retry1.log` | 1 passed, 493 filtered | Pre-fix retry; intermittent parent failure remains disclosed. |
| `evm-all1.log` | **291 passed, 0 skipped**, 13 binaries | All common-evm/common-evm2 unit and integration targets, not library-only; includes both new regressions. |
| `evm-all2.log` | **291 passed, 0 skipped**, 13 binaries | Explicit `--all-targets` confirmation of the same complete common-EVM scope. Overlaps evm-all1 entirely. |
| `candidate1.log` | **516 passed, 0 failed, 1 skipped** | Unfiltered six-library/four-integration candidate scope including strengthened offline snapshot copy/replay. |

Targeted counts overlap the full runs and must not be added as unique tests. The existing ignored RPC WebSocket test still requires a localhost node. The unrelated randomized consensus transaction-content test passed this run; its historical flake remains disclosed and its source is untouched. `git diff --check` passes. No fmt, Clippy, zepter, reth/pin or upstream API changes, other-repo writes, new review round, push/PR, deployment or benchmark. Supported-host CLI/custom-builder compilation and the previously documented unsupported recovery/service boundaries remain unchanged. Parent owns final independent verification.

## Native candidate ownership continuation from ba236505ac

This correction starts on draft PR #6591's local HEAD `ba236505ac`. No push or external action was performed. The approved design document remains byte-unchanged. The parent explicitly approved the following implementation ownership boundary for this bench/devnet PoC before production edits.

### Supported selection boundary and residual limitation

- Every controlled native payload carries a clone-shared lease. Replacement of an internal Better candidate, losing resolve candidates, job abandonment and the last never-exported payload drop retire only that identity's Builder provenance. Reading `block()` does not select a candidate; pinned BasicPayloadJob reads it for ordinary build notifications.
- Engine getPayload envelopes V1-V5, ExecutionData conversion, `into_sealed_block`, and successful `executed_block` extraction select the identity before exporting it. Normal Engine RPC/CL flow uses these boundaries. Merely resolving a job without exporting the result does not pin it.
- Arbitrary raw `block()` cloning and later import outside those boundaries receives normal independent imported-block validation, but is outside builder cross-comparison ownership coverage. This is an explicit supported-boundary limitation, not a claim that any raw block clone is tracked.
- An exported getPayload candidate abandoned by a later FCU remains protected. FCU does not prove it can never be imported or become a same-height fork. Configured retention/admission therefore bounds outstanding exported-unmatched comparisons and fails closed at capacity. Unlimited abandoned getPayload support is NOT claimed. The export/drop test models that ownership condition; it is not a live CL FCU-churn test.
- The manager serializes last-owner retirement with worker publication. Never-exported late completion cannot restore a retired root; provider work and its deadline remain owned until completion/drain. Reacquisition of the same pending identity revives its shared ownership without launching a second builder operation.
- Duplicate pending Builder identity is idempotent only after exact finalized hashed-state equality at native registration. Different input for the same sealed hash fails closed. Validator execution/root calculation is never coalesced or certified by this ownership mechanism. Existing real R/D root comparison and actual-parent DB fallback are retained.
- Successful S insertion still retires only after the inner hook succeeds. Failed insertion preserves its exported Builder comparison. Real validator publication releases ownership metadata only after independent comparison. At full retention, a real matching validator may reuse its own compared Builder slot; aging out accepted Validator data cannot strand previously matched Builder data as a new protected comparison.
- Never/pre-F allocate neither candidate leases nor the manager. No reth patch, schedule change, dependency, unsafe code, or new runtime task was added.

### Failing-first and final verification

New numbered evidence directory: `target/deferred-root-candidate-lifecycle1/`. Exact commands are in `evidence-commands1.txt`; no existing evidence was overwritten. All Rust commands use RUST_MIN_STACK=8388608 and scoped Justfile test-ci equivalents with nextest CI profile, locked/all-features/ci Cargo profile, zero retries and no fail-fast.

| Evidence | Exact result | Interpretation |
|---|---|---|
| `red1.log` | **0 passed, 2 failed, 19 skipped by filter** | Original production HEAD with tests only. Real native S/R/D abandoned-candidate test reaches capacity failure in S before the other cohorts; duplicate D native sealed identity fails pending admission and its explicit assertion. |
| `red2.log` | **0 passed, 1 failed, 20 skipped by filter** | Original production HEAD. Actual pinned BasicPayloadJob replacement of native Better candidates hits protected-ready exhaustion, then fails the rebuild assertion. Follow-on poisoned-fixture panics are consequences of that first real capacity failure, not separate defects. |
| `green1.log` | **269 passed, 0 skipped** | Initial payload/node libraries plus native semantic integration scope. Overlaps later runs. |
| `green2.log` | **524 passed, 1 skipped** | Six selected libraries and four integration targets before the final late-builder retention regression. Overlaps green3. |
| `green3.log` | **525 passed, 0 failed, 1 skipped** | Final six-library/four-integration scope, including all nine added regressions and existing S insert-success/failure, R/D real MPT comparison, same-height forks, actual-parent fallback, Never/pre-F parity, populated prewarm, snapshot replay and real runner Engine flow. No warnings. |

The new native loops use ready=2 and five distinct never-exported same-parent candidates, strictly exceeding twice the configured capacity. This exercises the same configurable capacity boundary as default ready=128 without 129 redundant disk-backed builds. The BasicPayloadJob test uses real native execution with a test adapter that returns Better to drive upstream ownership/replacement; it does not substitute a fake manager. Native pending-identity testing blocks the real worker, seals exactly the same hash twice, and asserts zero additional admitted root operations and zero Validator certification.

Additional new tests cover every supported envelope/export conversion, protected exported capacity exhaustion, late completion after drop/reacquisition, changed finalized duplicate input, comparison at capacity preserving an unmatched same-height fork, and late builder comparison after independent validation. The existing `unmatched_builder_provenance_remains_protected` contract remains unchanged.

Counts overlap and must not be summed. The one skipped pre-existing RPC WebSocket test requires localhost:8546. The unchanged randomized consensus transaction-content test passed this final run; its historical flake remains disclosed. `git diff --check` passes. No fmt, Clippy, zepter, broad clean, extra review round, CLI/custom-builder full package build, production/remote action, push or design-doc change. Supported-host compilation, parent CI/public-thread work and previously documented unsupported recovery/service boundaries remain outstanding.

## Slow native Better replacement queue correction from 550d0f1bda

Parent verified that last never-exported lease drop left queued closures and pending deadlines, so slow actual root computation plus ordinary BasicPayloadJob Better churn exhausted bounded admission without a root error. The approved design document is unchanged. Parent approved the following local queue correction before production edits, conditional on a genuine native failing-first test.

- Replace non-removable sync_channel work admission with a bounded owned VecDeque and Condvar. Last never-exported drop removes only still-queued matching work and its pending deadline/candidate metadata. Its closure never invokes the provider. Once worker pop owns an operation, it remains non-preemptible, pending and deadlined until completion/drain.
- Queued cancellation permits fresh-generation admission; running reacquisition remains idempotent. Worker pop and cancellation serialize through the queue lock, so no removed old closure can later erase new-generation pending work. Publication/completion serialize with candidate ownership.
- Atomic admission uses candidates -> queue -> builders lock order, bounded queue and bounded pending counts. Ordinary transient fullness backpressures, rather than marking unhealthy immediately. Waits release every other lock, preserve the original absolute deadline, and recheck after wakeups. Pop/cancel/complete/close wake admission. Completion holds the queue lock during pending removal/notification to prevent lost wakeups. True deadlines, generic completion abandonment, panic and drain failures remain terminal.
- No selected/exported work is canceled or skipped, and no validator computation is coalesced. Exported-unmatched comparisons remain protected/fail-closed at capacity. Raw block clones remain outside builder cross-comparison ownership but receive normal independent imported validation. Existing S insertion success/failure, c1-c4, same-height fork, actual-parent fallback, Never/pre-F and failstop/drain contracts remain.
- `admitted` counts admissions including later cancellation. New `computed` counts only invoked worker operations. Computation-start/completion metrics exclude canceled unused jobs. Cancellation scans only the fixed bounded queue, never pool-sized data. No new dependency/helper crate, reth patch or priority scheduling.

Fresh evidence directory: `target/deferred-root-queue1/`. Exact commands are in `evidence-commands1.txt`. All Rust runs used bg_process and scoped Justfile test-ci equivalents, locked/all-features/ci profiles, zero retries and no fail-fast.

| Evidence | Exact result | Interpretation |
|---|---|---|
| `red1.log` | **0 passed, 1 failed, 23 filtered** | Tests-only change before production edits. Real BasicPayloadJob cannot reach eight distinct native Better builds while a real MPT operation occupies the worker with queued=2. This setup fills the queue before isolating the separate immediate builders-length guard; it is genuine native queue-orphan red, not claimed as isolated guard-branch red. |
| `green1.log` | **276 passed, 1 failed, 0 skipped** | Queue units and new native replacement assertions passed. Its added independent validation failed because blocking fixture submit ran inside Tokio, corrected with spawn_blocking. This fixture failure remains recorded. |
| `green2.log` | **530 passed, 0 failed, 1 skipped** | Final six-library/four-integration scope, prior 525 plus five new tests and one updated test. No warnings. |

Native test asserts eight actual Better builds while a real MPT operation is blocked, exactly one queued current best, one pending identity, one ownership entry and only the running operation computed. Release computes exactly that real slow root and selected native builder root, never seven orphan provider roots. Real independent Engine validation then accepts the exact selected sealed identity and matches its actual MPT root. Four manager tests cover running reacquisition, cancellation/admission wake without retained ownership locks, protected-full backpressure/pop wake without skipping selected computations, and shutdown wake/drain. Updated queued-reacquisition testing verifies fresh admission and zero computation for both canceled generations.

Final parent verification scope (530 passing tests plus existing skipped localhost RPC test):

```sh
RUST_MIN_STACK=8388608 cargo nextest run -P ci --locked \
  -p base-execution-payload-builder -p base-execution-rpc -p base-node-core \
  -p base-node-runner -p base-execution-chainspec -p base-consensus-engine \
  --lib --test deferred_state_root_gate --test deferred_state_root_semantics \
  --test in_memory_engine --test rpc_baseline_parity \
  --all-features --cargo-profile ci --retries 0 --no-fail-fast
```

Counts overlap; do not sum earlier runs. Existing randomized consensus-content test passed, with historical flake still disclosed. `git diff --check` passes. Parent-reported Linux Format/Rust failures at 550 remain opaque (`Run clippy`, exit101); no concrete diagnostics/Depot authentication were obtained here, and no speculative CI source fixes were made. No fmt, Clippy, zepter, other-repo edits, extra review rounds, push, external interaction, deployment, benchmark, full CLI/custom-builder package build or approved-design edit occurred. Running providers remain non-preemptible. Prior unsupported recovery/export/service boundaries and supported-host compilation requirements remain. Parent owns independent final verification and CI diagnosis.
