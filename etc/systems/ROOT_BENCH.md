# Deferred root Engine benchmark contract

This command benchmarks serialized native payload construction and first import on an isolated custom chain. It is not a consensus service, derivation test, independent-node throughput test, or performance certification. The implementation and acceptance checks below require supported-host verification before measurement.

## Commands

```sh
base-bench root-bench freeze --output workload1.json --blocks 8 --transactions-per-block 10 --warmup 1
base-bench root-bench run --input workload1.json --cohort r --output trial1 --executable-manifest build1.json
```

Run one process per cohort/trial. `--cohort` accepts `o`, `s`, `r`, `d`. `--call-timeout-ms`, `--run-timeout-ms`, and `--shutdown-timeout-ms` bound calls, the trial including launch, and graceful drain respectively. No call retries or artificial sleeps are allowed. The output basename must end in a number; existing destinations are refused. `trial1/datadir1` is disposable; never remove its sibling experimental marker to reuse it.

Fresh genesis only is supported. Snapshot restore is not implemented here; the existing production admission manifest/API is the required route for a future separately tested extension. There is no snapshot rewrite or active-database recovery path.

## Frozen input

Versioned JSON contains `chain_id: 900`, genesis allocation, activation timestamp 2, `warmup`, and ordered `blocks` containing complete `BasePayloadAttributes`, including signed EIP-2718 `transactions`, fixed timestamps, gas limit, randomness, fee recipient, withdrawals, beacon root and fee parameters. `digest` is Keccak of the canonical serde JSON encoding with that field zeroed. Preparation uses `SubmissionPipeline::sign_at_nonce` with an explicitly funded deterministic test account. Run validates digest, signed chain ID, recovered sender, contiguous nonce, gas, fees, recipient and value before any writable node launch. Signing and serialization are outside measured phases. Input must be treated as immutable by base-metal and independently content-addressed in its trial manifest.

This first workload is state-changing native ETH transfers, a correctness/latency smoke workload, not representative root-performance coverage. It does not depend on ancestor hashes.

These are deliberate EL-only blocks without an L1-info transaction. Receipt verification uses the launched node's canonical execution provider `receipts_by_block(accepted_hash)`, not Base's `eth_getTransactionReceipt` converter, which requires L1-info calldata. The accepted canonical head hash and native payload transaction bytes/order bind the block; native receipts are positionally associated with that exact block's transactions. Exact receipt count, success status, per-position cumulative-gas delta of 21000 and total header gas are required. Native receipts do not have RPC transaction/block/index metadata. Sender nonce and recipient balance remain exact checks. No receipt verification is skipped and no L1/CL fixtures are inserted.

## Cohort launch

All arms use CLI-derived `ExecutionNodeArgs`, typed `ExecutionNodeLaunchConfig::launch_default`, disposable acknowledgement, chain 900, no CL/L1 services, no forwarding, no proof history, no metering/indexer/payer/event extensions, no discovery and no peers. The witness diagnostic hook is explicitly unconfigured. Queue 6, retention 128, root timeout 30000 ms, persistence threshold 0, allow-unwind-canonical-header true, snap-v2 false and unused ports are common. Only D sets `config.base.deferredStateRoot = 2`. O selects `ordinary`, S `serial-trusted`, R `serial-reexecution`, D `deferred`.

O is candidate ordinary, NOT immutable #6268. An unchanged #6268 baseline needs a separately built, attested harness-only adapter because it lacks the admitted extension-free cohort runner. No baseline binary or parity certification is supplied here. D vs R is the primary comparison; D vs O confounds algorithm and re-execution.

Each build extends its own cohort's last accepted parent, never a frozen parent hash. The sequence is build FCU, immediate getPayload, first newPayload, canonical FCU. All three status-bearing responses require VALID and exact latestValidHash. Payload transaction bytes/order and canonical receipts are checked. R and active D additionally require exact per-block deltas of builder execution/current root, validator execution/current root, ready validator output, and zero trusted insertion attempts. VALID by itself can mean AlreadySeen and does not pass this gate. Pre-F D does not emit controlled execution accounting. O/S trusted insertion event ordering remains observable rather than guaranteed by sleeps.

## Output

`input.json` preserves the canonical input. `launch.json` records effective debug configuration, receipt source, arguments and BASE_/RETH_ environment. Its `runtime_checkout` commit/tree/lock/pin identity describes only the runtime working directory, never the executable's source. Sensitive environment values are hashed. `executable.json` identifies executable and compiled benchmark-module bytes; `executable-source.json` retains the supplied external manifest. See the provenance gate below.

`samples.jsonl` is flushed after each block cycle or call failure, including raw monotonic start/end nanoseconds, payload ID, parent/block hash, warmup membership, responses/status/latestValidHash, header commitment, error, gas and successful receipt count. Successful Engine responses are retained as typed pending values, then serialized after canonical FCU or on any error/cancellation. No disk writes or response serialization delay successful Engine phases. `metrics-before.txt`, per-block `metricsN-before.txt`/`metricsN-after.txt`, and `metrics-drained.txt` preserve raw `base_root_experiment` Prometheus lines without distributing deltas over blocks.

`result.json` records completion/error, drain status, samples and nearest-rank count/p50/p90/p95/p99 summaries. `drain.json` records graceful guard completion separately from observed healthy, joined root worker state, manager presence, and consumed critical-task result. All active S/R/D trials require the shared manager, closed queue, no retained worker JoinHandle and healthy manager after guards finish. O may have no manager. The runtime critical-task result is consumed during execution or shutdown, but its normal shutdown result alone cannot prove worker completion because the task manager can exit before drain-task panics.

Partial/error artifacts remain on launch, call, validation, Ctrl-C cancellation and drain failure; nonzero exit means incomplete. Abrupt termination can lose the current in-memory block sample. SIGTERM-specific cleanup is not yet verified.

Receipt provider tasks run only on the explicitly owned execution runtime. Their handles remain retained across call and trial cancellation; `drain.json.receipt_reads_completed` is false while any such operation is still running. A timeout does not cancel an indivisible provider read. Runtime shutdown is bounded, and unfinished work makes the trial incomplete.

Run from any directory inside a runtime source checkout. The harness resolves its Git root for diagnostic lock/pin reads. That checkout is not the executable-source attestation.

Phase names are `build_fcu`, `get_payload`, `new_payload`, `canonical_fcu`, `payload_available` and `cycle`. FCU duration alone is admission, not construction time. `payload_available` spans build FCU request to getPayload response. `cycle` spans build request to canonical acceptance. Forced-attribute submission-to-inclusion is not mempool ingress latency. Warmup samples remain raw but are excluded from summaries and accepted-throughput numerators. Throughput divides accepted measured gas and successful included receipt count by measured wall time through completed shutdown/drain, not offered gas/transaction count. Failed or absent samples are never zero-valued successes.

## Exact metric-family contract

All families below are exported with the `reth_` prefix by Reth's shared recorder, so `base_root_experiment_jobs_started` is emitted as `reth_base_root_experiment_jobs_started`. Apply that prefix to every family in the tables and lists below. Counters have no `_total` suffix with this exporter configuration. The harness shares Reth's idempotent recorder installation rather than installing a competing global recorder.

| Counter | Meaning |
|---|---|
| `base_root_experiment_jobs_started` | Actual serial provider operations invoked, including parent recovery |
| `base_root_experiment_jobs_completed` | Invoked operations completed, including failures |
| `base_root_experiment_jobs_failed` | Provider failure or panic; zero alone does not certify drain |
| `base_root_experiment_current_roots_started` | Current roots by `source="builder"` or `source="validator"` |
| `base_root_experiment_builder_executions` | Controlled builder attempts |
| `base_root_experiment_validator_executions` | Independent controlled validator attempts |
| `base_root_experiment_validator_ready_outputs` | Ready outputs exposed to Engine, not canonical acceptance |
| `base_root_experiment_trusted_insertions` | Controlled trusted-insertion attempts |
| `base_root_experiment_redundant_stock_sorts` | Redundant stock sorting still owned by runtime |
| `base_root_experiment_payloads_sealed` | Immutable sealed payloads, not Engine VALID responses |

Distribution families, all durations in seconds:

- `base_root_experiment_job_duration`
- `base_root_experiment_queue_lag`
- `base_root_experiment_completion_wait`
- `base_root_experiment_sorting_duration`
- `base_root_experiment_calculation_duration`
- `base_root_experiment_builder_execution_duration`
- `base_root_experiment_validator_execution_duration`
- `base_root_experiment_finalization_to_seal`

These are recorded with the metrics histogram API but the default Prometheus builder has no configured histogram buckets. It exports summaries with `{quantile="..."}` series and `_sum`/`_count`, not guaranteed `_bucket` series. Preserve the emitted TYPE and labels; do not reinterpret quantile values as buckets or subtract quantiles to infer per-block percentiles. Metrics have process scope. A one-process-per-trial policy is required. The harness is included in that process's resource cost.

An absent counter is treated as zero only for exact counter gates. Required active R/D deltas must still be exactly one, so empty scrapes cannot pass. Missing distributions mean no observations, not zero latency. Read `result.json.complete` and process exit status first; never aggregate incomplete trials, `error` rows, missing phases, or warmup rows into successful observations. JSONL contains successive snapshots: retain the last row per block number rather than summing duplicate failure snapshots. Empty latency summaries have `count: 0` and null percentile fields. Failed trials may retain diagnostic partial throughput values; these are not measurements.

## Executable provenance gate

`--executable-manifest` is mandatory for measurement. An external trusted build operator must supply a JSON object with `executable_keccak` matching the exact running executable's bytes, `source_commit` and `source_tree` as 40-digit Git object IDs, `cargo_lock_keccak`, `reth_pin_keccak` as 32-byte Keccak hex digests, and `clean_source: true`. The manifest must be produced from the clean build checkout and final executable by the external build process, not inferred from runtime cwd. Keep it outside the binary-owned trial directory. No broad build system is added here.

The command checks schema and executable digest binding and preserves the external attestation. It does not independently prove the builder's truthfulness or authenticate the manifest issuer. Operator trust and correct source-to-binary attestation remain external responsibilities. Missing, malformed, dirty-source or mismatched attestation refuses launch and measurement success. A matching digest alone without source/lock/pin fields is insufficient. Runtime checkout identity need not equal the attested build identity and cannot replace it.

The actual typed-launch integration test also requires `ROOT_BENCH_EXECUTABLE_MANIFEST` pointing to an externally generated manifest for that exact test executable, after building it on the supported host. Do not generate an attestation from arbitrary current checkout state simply to make the gate pass.

That integration test is explicitly ignored by default because it needs external provenance and process-global metric isolation. Run unit tests separately, then launch the built test executable in its own process with the supplied manifest, filtering only `root_bench::tests::admitted_nonempty_r_d_engine_trial --ignored --exact --test-threads=1`. Do not combine it with injected worker-failure tests in the same process. The top-level `just test` excludes `base-system-tests`, so targeted package commands are required for this module.

## External responsibilities and unverified requirements

base-metal owns paired repeated trials, CPU affinity, OS CPU time/RSS/I/O sampling and resource accounting. Queue-depth/cache-hit/miss/persistence-lag/cancellation and paced/missed-cadence measurements are unsupported here. Serialized saturation has no configured cadence and proves no pipeline overlap. A shutdown timeout is incomplete, not evidence that an indivisible provider thread stopped.

Required gates before use: package compile and the colocated `root_bench::tests` on a supported host; actual admitted R/D nonempty activation runs; core semantic/publication/retention regressions; independent first-import metrics; exact state/receipt/nonce/gas parity; immutable baseline parity and source provenance; O/S insertion-path observations; retention stress beyond 128 roots; failure injection for worker panic/drop/timeout and cancellation. No full performance certification is implied by source implementation or unit checks.
