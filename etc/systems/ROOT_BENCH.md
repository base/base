# Deferred root Engine benchmark contract

This command measures serialized native payload construction and first import on an isolated custom chain. It is not a consensus service, derivation test, independent-node throughput test, or performance certification. Source implementation and mocked tests do not replace supported-Linux execution.

## Commands and preconditions

Build on the operator's Linux host from a clean, pinned build checkout:

```sh
cargo build --locked --release -p base-system-tests --bin base-bench
```

With `target/release` on `PATH`:

```sh
base-bench root-bench freeze --output workload1.json --blocks 8 --transactions-per-block 10 --warmup 1
base-bench root-bench run --input workload1.json --cohort r --output trial1 --executable-manifest build1.json
```

Run one process per trial/cohort. Parent directories must already exist and be writable. Output basenames must end in a number; freeze files and trial directories are create-new, never reused. `trial1/datadir1` is disposable; never remove its sibling experimental marker to reuse it. Keep orchestration inputs, the external manifest and operator evidence outside the binary-owned output directory. Run inside a Git source checkout containing `Cargo.lock` and `etc/upstream-pins/reth.toml`; the resolved checkout supplies diagnostic identity, not executable provenance. Only one active trial per process is admitted.

| Command | Flag | Type | Default | Enforced contract |
|---|---|---|---|---|
| freeze | `--output` | path | required | new numbered JSON basename; existing parent |
| freeze | `--blocks` | u64 blocks | 8 | 4..=10000, including warmup |
| freeze | `--transactions-per-block` | u64 transfers | 10 | 1..=1000 |
| freeze | `--warmup` | u64 blocks | 1 | 0..blocks, exclusive upper bound |
| run | `--input` | path | required | readable validated frozen bundle |
| run | `--output` | path | required | new numbered directory; existing parent |
| run | `--cohort` | enum | required | `o`, `s`, `r`, `d` |
| run | `--executable-manifest` | path | required by Clap | external clean-source attestation for exact executable |
| run | `--call-timeout-ms` | u64 milliseconds | 30000 | positive; each launch/Engine/read wait |
| run | `--run-timeout-ms` | u64 milliseconds | 300000 | positive; launch, saturation and post-loop verification, excluding drain |
| run | `--shutdown-timeout-ms` | u64 milliseconds | 30000 | positive; each shutdown stage, not a combined shutdown budget |

No call retries or artificial sleeps. Only fresh genesis is supported. Snapshot restore, snapshot rewriting and active-database recovery are not implemented. The existing production admission manifest/API is the route for any separately tested extension.

## Input and executable provenance

Preparation uses `SubmissionPipeline::sign_at_nonce` to freeze signed state-changing native ETH transfers, not a representative root-performance workload. The well-known synthetic test-only signing key is 32 repeated `0x01` bytes. It is public, not a credential. Never use it for real funds.

`input.json` preserves the exact input file bytes, including whitespace and key order. Its raw-byte digest is not the bundle's canonical integrity digest. Validation checks the canonical digest, exact genesis/forks, chain, fixed attributes, recovered sender, contiguous nonce, gas, fees, recipient and value before node launch. Signing and serialization are outside measured Engine calls. Input should also be independently content-addressed by the operator.

All bundle fields are required and non-null; unknown bundle fields reject:

| Input field | JSON type / units | Contract |
|---|---|---|
| `version` | u64 | 1 |
| `chain_id` | u64 | disposable chain 900 |
| `genesis` | Ethereum Genesis object | timestamp 0, exact supported allocation/forks; no common D schedule |
| `activation` | u64 Unix seconds | 2 |
| `warmup` | u64 blocks | less than block count |
| `blocks` | array of BasePayloadAttributes | 4..=10000 ordered blocks |
| `digest` | 32-byte hex | Keccak of canonical serde JSON with digest zeroed |

Each attributes object uses the native Engine serde schema: `timestamp` is a hex quantity in seconds, contiguous 1..N; `prevRandao` is zero bytes32; `suggestedFeeRecipient` is zero address; `withdrawals` is an empty array; `parentBeaconBlockRoot` is zero bytes32. Optional Engine `slotNumber` and `targetGasLimit` are absent/null. `transactions` is a non-null ordered array of 1..=1000 EIP-2718 hex byte strings; `noTxPool` is true; `gasLimit` is 30000000; `eip1559Params` is bytes8 encoding denominator 50/elasticity 6; `minBaseFee` is 1 wei. Run validates the exact serialization against the supported attributes constructor, not arbitrary Engine options.

`--executable-manifest` must be supplied by a trusted external build operator from the clean build checkout and final executable, not inferred from runtime cwd. Keep it outside the output directory. It is a JSON object with required non-null fields:

| Manifest field | Type / units | Check |
|---|---|---|
| `executable_keccak` | 32-byte hex | Keccak of exact running executable bytes |
| `source_commit`, `source_tree` | 40-digit Git hex IDs | structurally valid build-source identities |
| `cargo_lock_keccak`, `reth_pin_keccak` | 32-byte hex | build lock and upstream-pin identity |
| `clean_source` | bool | true |

Malformed, missing, dirty-source or mismatched attestation refuses launch. Digest binding and schema do not authenticate the issuer or prove the builder's truthfulness. Runtime checkout identity need not equal build identity and cannot replace attestation. No broad build system is added.

## Launch and successful-loop boundary

All arms use CLI-derived `ExecutionNodeArgs` and typed `ExecutionNodeLaunchConfig::launch_default`, disposable acknowledgement, chain 900, unused ports, no CL/L1 services, forwarding, proof history, metering/indexer/payer/event extensions, discovery or peers. Witness hooks are explicitly unconfigured. Queue 6, retention 128, root timeout 30000 ms, persistence threshold 0, allow-unwind-canonical-header true and snap-v2 false are common. Every cohort explicitly requires `node_config.prune_config().is_none()` before launch and records `config.unpruned: true`. This does not change pruning implementation. Unpruned history beyond the ready-cache horizon still needs runtime verification.

O selects candidate `ordinary`; S `serial-trusted`; R `serial-reexecution`; D `deferred`. Only D installs `config.base.deferredStateRoot = 2`. O is NOT immutable #6268. An unchanged #6268 baseline needs a separately built/attested harness-only adapter because it lacks this admitted extension-free runner. D versus R is the primary comparison; D versus O confounds algorithm and re-execution.

The production-used `RootBenchEngine` trait has only FCU V3, getPayload V4 and newPayload V4, with a direct `SharedEngineApi` implementation and Mockall default tests. It is benchmark-local, not a general Engine framework.

Before saturation, the command validates/launches, reserves sample capacity, clones owned attributes, precomputes cumulative transaction counts and captures baseline metrics. Saturation performs only:

1. FCU with owned attributes extending this cohort's last accepted parent.
2. Immediate getPayload, with no scheduling sleep.
3. First newPayload, with its consumed payload clone prepared before that call's clock.
4. Canonical FCU, then immediately the next build.

All three status-bearing responses require VALID and exact latestValidHash; build FCU requires a payload ID. Failure stops without retry. Complete typed envelopes, response objects, advertised hashes and raw call boundaries remain in memory. No block reconstruction/header clone, transaction comparison, block RPC, provider work, metric rendering, JSON serialization or filesystem work occurs in the successful loop or between canonical FCU and the next build.

After the entire loop, verification reconstructs sidecar-complete native headers and verifies advertised hashes, parent/number/timestamp and transaction bytes/order. D at activation must commit the actual pre-F parent's post-state root, not the current block's root. After activation the parent's delayed header root must not be used as actual parent state; this harness only asserts the explicit activation boundary and otherwise relies on core semantics.

Each retained accepted hash is checked against its canonical block by NUMBER; a separate Latest read checks the final head. Receipts and accounts are read by ACCEPTED HASH on retained blocking tasks on the explicitly owned execution runtime. EL-only blocks have no L1-info transaction, so this never uses Base's enriched `eth_getTransactionReceipt` converter or `eth_getTransactionCount`/`eth_getBalance`. Native receipts require exact cardinality, success, per-position cumulative-gas delta 21000 and total header gas. Sender nonce and recipient balance equal the precomputed cumulative transfer count. Native receipts have no RPC transaction/block/index metadata; positional association is bound to the exact accepted block transaction order.

## Accounting and timing

Only baseline and post-drain metric snapshots are taken. R requires exact aggregate execution/current-root/ready-output deltas for every workload block. D requires them only for timestamps at or after activation. Warmup remains included in correctness/accounting: timestamps 1..4 with activation 2 require R=4 and D=3. Trusted insertion attempts must stay zero. VALID alone may mean AlreadySeen and is not independent-work evidence. Aggregates cannot independently certify every hash: excess work for one hash can mask missing work for another. O/S insertion event order remains observable, not guaranteed by sleeps.

Phase boundaries are actual monotonic nanoseconds relative to trial origin. `build_fcu`, `get_payload`, `new_payload`, `canonical_fcu` each have raw start/end. FCU alone measures admission, not construction. `payload_available` is the observed span from build FCU request start to successful getPayload response, including inter-call overhead. `cycle_ns` is exactly the sum of the four Engine phase durations, NOT that wall-clock span. There is no fabricated `cycle` endpoint; summaries named `cycle` use `cycle_ns`.

Warmup/error/absent-cycle observations are excluded from throughput numerators and cycle percentiles. Empty summaries have count 0 and null percentiles. Throughput is gas/s and successful receipts/s, dividing verified measured totals by wall time from the first measured build through runtime shutdown/drain. This includes post-loop verification of ALL blocks, failure evidence writes during cleanup and shutdown overhead. It is not Engine-only saturating throughput and makes no claim that moving filesystem work eliminates dilution. Final post-drain result/sample writes are outside that denominator. No separate Engine-only throughput is emitted. Missing start, nonpositive elapsed time or no eligible samples yields null, never infinity or invented zero observations. Partial rates on incomplete results are diagnostics, not measurements. Forced-attribute submission-to-inclusion is not mempool ingress latency.

## Artifacts and schemas

Files are create-new except append-only `samples.jsonl`. Numbers below are JSON integers unless stated otherwise. Engine responses/envelopes use native serde Engine schemas with hex quantities and hashes. Fields called nullable may be null when unavailable; missing files are possible on refusal or filesystem failure.

| Artifact | Fields and types / units |
|---|---|
| `input.json` | original raw bytes, schema above |
| `executable-source.json` | `external_manifest`: object or null; `verification`: string `external-attestation-required`; `executable_keccak`: bytes32 hex |
| `executable.json` | `executable_keccak`, `harness_module_keccak`: bytes32 hex |
| `launch.json` | `cohort`: O/S/R/D string; `transport`: string `typed-in-memory`; `receipt_source`, `account_source`: provider-method description strings; `config`: object below; `runtime_checkout`: object below; `argv`: array of hashed strings; `environment`: name-to-hash object; `unsupported`: array of strings |
| `launch.config` | `unpruned`, `allow_unwind_canonical_header`, `snap_v2`, `disable_discovery`, `witness_hook_configured`: bools; `queue`, `retention`: counts; `persistence_threshold`: blocks; `max_inbound_peers`, `max_outbound_peers`: nullable counts, configured 0 |
| `launch.runtime_checkout` | `source`: string containing diagnostic commit/tree IDs; `cargo_lock_keccak`, `reth_pin_keccak`: bytes32 hex; `not_executable_provenance`: true |
| `runtime.json` | `configuration`: string of default RuntimeConfig Debug, not CLI/environment Debug |
| `metrics-before.txt`, `metrics-drained.txt` | raw Prometheus root-experiment families/metadata, process scope |
| `samples.jsonl` | successive sample objects below; retain last row per block number, never sum duplicate failure snapshots |
| `drain.json` | `drained`, `guards_completed`, `worker_joined_healthy`, `receipt_reads_completed`, `manager_observed`: bools; `critical_task_result`: diagnostic string |
| `result.json` | `complete`, `drained`: bools; `error`: nullable string; `cohort`: O/S/R/D string; `input_digest`: bytes32 hex; `samples`: sample array; `summaries`: phase-name-to-summary object; `measurement_start_ns`: nullable u64 ns; `drain_end_ns`: u64 ns; `accepted_gas_per_second`, `successful_receipts_per_second`: nullable f64 gas/s and receipts/s |
| refusal `result.json` | best-effort subset `complete:false`, `drained:false`, `error`: string, `cohort`: string; other fields absent |

Each sample has `number`: one-based u64 block index; `warmup`: bool; `parent_hash`: bytes32 hex; `block_hash`: nullable bytes32 hex; `phases`: name-to-`{start_ns:u64,end_ns:u64}` object in ns; `cycle_ns`: nullable u64 ns; `responses`: object with optional `build_fcu`, `new_payload`, `canonical_fcu`, complete `payload` envelope and reconstructed `payload_header`; `error`: nullable string; `accepted_gas`: u64 gas; `successful_receipts`: u64 count. The last two remain 0 until verified and must not be interpreted without completion/error gates. Typed `pending` is in-memory only, not a JSON field. Summaries have `count`: usize observations and nullable u64 ns `p50_ns`, `p90_ns`, `p95_ns`, `p99_ns` using exact nearest rank.

Every captured BASE_/RETH_ environment value and every whole argument token is Keccak-hashed, including benign values, endpoints, headers, JWT settings, paths and inline flag values. No broad CLI-derived configuration dump is retained. Hashes do not replace executable provenance.

Serialization, persistence and join errors cannot skip cleanup. Retained provider handles survive cancellation/timeouts and are consumed if finished. Graceful guard completion alone does not prove worker completion: active S/R/D requires the actual manager, closed queue, no retained worker JoinHandle and healthy manager. O may have no manager. Critical-task results are consumed, but a normal task-manager result alone is insufficient because drain-task panics may occur later. Post-drain worker-failure metrics must remain zero. An indivisible provider operation is not cancelled by its caller's timeout; outstanding work prevents success. Explicit runtime shutdown remains bounded. Abrupt termination can lose current in-memory evidence; failed filesystems can prevent all artifacts. SIGTERM cleanup is not verified.

## Exit categories

Only the thin `base-bench` RootBench dispatch uses these categories. Other commands retain existing behavior.

| Exit | Meaning |
|---|---|
| 0 | complete, verified, drained success |
| 2 | refused before execution, including CLI/preflight/input/provenance/output admission |
| 3 | active incomplete trial without an observed harness deadline |
| 4 | actual harness launch/call/read/run/shutdown deadline expiration |

Typed categories survive appended evidence/cleanup/filesystem errors. An observed harness timeout takes precedence even if a non-timeout failure occurred first. Otherwise the primary category is retained. Opaque internal root errors are incomplete/3, even if their text says timeout: core worker errors erase their underlying type. No substring matching. A false graceful-shutdown completion result establishes an observed shutdown deadline; a worker health failure by itself does not.

## Exact metric families

Reth's shared idempotent recorder emits the full names below, without `_total`. An absent counter is zero only for exact counter gates; missing active deltas cannot pass. No per-block scrapes or metric-derived per-block percentiles are emitted.

| Counter | Meaning / labels |
|---|---|
| `reth_base_root_experiment_jobs_started` | actual provider operations, including parent recovery |
| `reth_base_root_experiment_jobs_completed` | invoked operations completed, including failures |
| `reth_base_root_experiment_jobs_failed` | provider failure/panic; zero alone does not certify drain |
| `reth_base_root_experiment_current_roots_started{source="builder"}` | controlled builder current roots |
| `reth_base_root_experiment_current_roots_started{source="validator"}` | controlled validator current roots |
| `reth_base_root_experiment_builder_executions` | controlled builder attempts |
| `reth_base_root_experiment_validator_executions` | independent controlled validator attempts |
| `reth_base_root_experiment_validator_ready_outputs` | ready outputs exposed to Engine, not canonical acceptance |
| `reth_base_root_experiment_trusted_insertions` | controlled trusted-insertion attempts |
| `reth_base_root_experiment_redundant_stock_sorts` | redundant stock sorts still owned by runtime |
| `reth_base_root_experiment_payloads_sealed` | sealed payloads, not VALID responses |

Do not require jobs_started = twice the active-block count: parent-root recovery also invokes provider operations. Duration families in seconds are `reth_base_root_experiment_job_duration`, `reth_base_root_experiment_queue_lag`, `reth_base_root_experiment_completion_wait`, `reth_base_root_experiment_sorting_duration`, `reth_base_root_experiment_calculation_duration`, `reth_base_root_experiment_builder_execution_duration`, `reth_base_root_experiment_validator_execution_duration`, `reth_base_root_experiment_finalization_to_seal`. Preserve emitted labels/TYPE. These use the histogram API but the default exporter has no configured buckets: output is summaries with quantiles, `_sum` and `_count`, not guaranteed `_bucket`. Never subtract quantiles. Process-global metrics require one isolated process per trial; the harness remains part of process resource cost.

## Linux operator gates and gap map

The operator owns execution; the commands below are unrun here. Top-level `just test` excludes this package, so use the exact scoped commands:

```sh
RUST_MIN_STACK=8388608 cargo test --locked -p base-system-tests \
  --lib root_bench::tests:: -- --test-threads=1
cargo test --locked -p base-system-tests --lib --no-run
cargo build --locked --release -p base-system-tests --bin base-bench
```

Have a trusted external operator attest the exact `base_system_tests-<hash>` executable printed by `--no-run`. Set its path as `TEST_EXECUTABLE` and the externally supplied manifest as `EXTERNAL_TEST_MANIFEST`. Do not rebuild after attestation. Run only this ignored integration test in its own process, separate from injected worker-failure tests:

```sh
ROOT_BENCH_EXECUTABLE_MANIFEST="$EXTERNAL_TEST_MANIFEST" \
RUST_MIN_STACK=8388608 \
"$TEST_EXECUTABLE" root_bench::tests::admitted_nonempty_r_d_engine_trial \
  --ignored --exact --test-threads=1
```

| Requirement | Existing implementation / test path | Evidence level and remaining gate |
|---|---|---|
| exact production Engine order, next parent, fail closed | `etc/systems/src/root_bench.rs`: `engine_driver_order_and_next_parent`, `engine_driver_fails_closed`, `every_phase_deadline_is_typed` | source/default mock and timeout-wrapper tests; Linux compile/run required; mocks return typed timeout errors, wrapper separately observes real deadlines |
| native sidecars/hash, activation, bytes/order, state/receipts | same module: `native_payload_sidecars_and_advertised_hash`, `activation_parent_root_and_payload_identity`, `native_receipt_verification`, `admitted_nonempty_r_d_engine_trial` | default source tests and ignored real-engine test; real MPT correctness and parity unverified |
| independent aggregate accounting/unpruned history | same module: `aggregate_active_counts`, `status_and_independent_accounting` | source/default tests; real R/D metrics and historical accepted-hash reads beyond 128 roots required |
| timing, summaries, warmup, rates | same module: `exact_cycle_and_throughput`, `percentiles_and_warmup` | pure production-used helpers; not a performance result |
| provenance, bytes, categories, lifecycle | same module: `executable_manifest_required_and_bound`, `input_bytes_are_verbatim`, `exit_categories_and_required_manifest`, cleanup tests | default tests unrun; actual filesystem failure/cancellation/SIGTERM gates remain |
| deferred semantics/publication/retention | `crates/execution/node/tests/deferred_state_root_semantics.rs`, `crates/execution/payload/src/post_state_root.rs`, `crates/execution/runner/src/deferred_state_root.rs` | existing core code/tests, not changed or rerun; F/F+1 actual-parent semantics and retention stress required |
| immutable baseline, snapshot, representative load | existing replay/snapshot modules in `etc/systems/src`, workload tooling in `crates/infra/load-tests` | not provided by this fresh-genesis transfer benchmark; separate baseline adapter/parity and workload gates |
| paired trials and resource/cadence accounting | external operator/base-metal | CPU affinity, OS CPU/RSS/I/O, paired repeats external; queue/cache/persistence/cancellation/paced/missed-cadence unsupported; serialized saturation proves no overlap |

### Behavioral red-on-revert, operator-owned and unrun

In an operator-owned scratch checkout keep the compileable Engine trait, adapter, driver and tests. Reverse ONLY one correction below, run the named exact test expecting its behavioral assertion failure, restore it and rerun green. Compilation/dependency failure, missing attestation, or reversing the entire extraction is NOT red evidence. Use new numbered logs for every attempt and never overwrite previous evidence.

| Specific reversible behavioral change | Exact default test filter |
|---|---|
| In `drive`, replace `Ok(hash) => parent = hash` with `Ok(_) => {}` | `root_bench::tests::engine_driver_order_and_next_parent` |
| In `require_valid`, remove only `&& status.latest_valid_hash == Some(hash)` | `root_bench::tests::engine_driver_fails_closed` |
| In `finish_cycle`, replace the successful four-phase sum with `Some(0)` while retaining the error guard | `root_bench::tests::exact_cycle_and_throughput` |
| In `verify_block`, remove only the D activation `ensure!` | `root_bench::tests::activation_parent_root_and_payload_identity` |
| In `active_blocks`, exclude warmup by adding `.skip(bundle.warmup as usize)` before `.filter` | `root_bench::tests::aggregate_active_counts` |
| In `execute`, replace the `input.json` raw write with `RootBenchBundle::write_new(..., &bundle)` | `root_bench::tests::input_bytes_are_verbatim` |
| In `retain_error`, replace only the combined `RootBenchFailure` with an untyped formatted `eyre!` | `root_bench::tests::exit_categories_and_required_manifest` |

For each table filter use:

```sh
RUST_MIN_STACK=8388608 cargo test --locked -p base-system-tests \
  --lib root_bench::tests::engine_driver_order_and_next_parent \
  -- --exact --test-threads=1
```

Substitute the exact filter from the table. Mandatory observed behavioral red/green is not established by this guide or static checks. No independent-node performance, immutable-baseline parity, actual delayed-root correctness, long-run history availability or Linux execution is certified here.
