# Deferred root Engine benchmark contract

This command benchmarks serialized native payload construction and first import on an isolated custom chain. It is not a consensus service, derivation test, independent-node throughput test, or performance certification. The implementation and acceptance checks below require supported-host verification before measurement.

## Commands

```sh
base-bench root-bench freeze --output workload1.json --blocks 8 --transactions-per-block 10 --warmup 1
base-bench root-bench run --input workload1.json --cohort r --output trial1
```

Run one process per cohort/trial. `--cohort` accepts `o`, `s`, `r`, `d`. `--call-timeout-ms`, `--run-timeout-ms`, and `--shutdown-timeout-ms` bound calls, the trial including launch, and graceful drain respectively. No call retries or artificial sleeps are allowed. The output basename must end in a number; existing destinations are refused. `trial1/datadir1` is disposable; never remove its sibling experimental marker to reuse it.

Fresh genesis only is supported. Snapshot restore is not implemented here; the existing production admission manifest/API is the required route for a future separately tested extension. There is no snapshot rewrite or active-database recovery path.

## Frozen input

Versioned JSON contains `chain_id: 900`, genesis allocation, activation timestamp 2, `warmup`, and ordered `blocks` containing complete `BasePayloadAttributes`, including signed EIP-2718 `transactions`, fixed timestamps, gas limit, randomness, fee recipient, withdrawals, beacon root and fee parameters. `digest` is Keccak of the canonical serde JSON encoding with that field zeroed. Preparation uses `SubmissionPipeline::sign_at_nonce` with an explicitly funded deterministic test account. Run validates digest, signed chain ID, recovered sender, contiguous nonce, gas, fees, recipient and value before any writable node launch. Signing and serialization are outside measured phases. Input must be treated as immutable by base-metal and independently content-addressed in its trial manifest.

This first workload is state-changing native ETH transfers, a correctness/latency smoke workload, not representative root-performance coverage. It does not depend on ancestor hashes.

## Cohort launch

All arms use CLI-derived `ExecutionNodeArgs`, typed `ExecutionNodeLaunchConfig::launch_default`, disposable acknowledgement, chain 900, no CL/L1 services, no forwarding, no proof history, no metering/indexer/payer/event extensions, no discovery and no peers. The witness diagnostic hook is explicitly unconfigured. Queue 6, retention 128, root timeout 30000 ms, persistence threshold 0, allow-unwind-canonical-header true, snap-v2 false and unused ports are common. Only D sets `config.base.deferredStateRoot = 2`. O selects `ordinary`, S `serial-trusted`, R `serial-reexecution`, D `deferred`.

O is candidate ordinary, NOT immutable #6268. An unchanged #6268 baseline needs a separately built, attested harness-only adapter because it lacks the admitted extension-free cohort runner. No baseline binary or parity certification is supplied here. D vs R is the primary comparison; D vs O confounds algorithm and re-execution.

Each build extends its own cohort's last accepted parent, never a frozen parent hash. The sequence is build FCU, immediate getPayload, first newPayload, canonical FCU. All three status-bearing responses require VALID and exact latestValidHash. Payload transaction bytes/order and canonical receipts are checked. R and active D additionally require exact per-block deltas of builder execution/current root, validator execution/current root, ready validator output, and zero trusted insertion attempts. VALID by itself can mean AlreadySeen and does not pass this gate. Pre-F D does not emit controlled execution accounting. O/S trusted insertion event ordering remains observable rather than guaranteed by sleeps.

## Output

`input.json` preserves the canonical input; `launch.json` records effective debug configuration, source commit/tree/lock/pin digests, arguments and BASE_/RETH_ environment. Sensitive environment values are hashed. `executable.json` identifies the executable and compiled benchmark module bytes. `samples.jsonl` is flushed after each block cycle or call failure, including raw monotonic start/end nanoseconds, payload ID, parent/block hash, warmup membership, responses/status/latestValidHash, header commitment, error, gas and successful receipt count. No disk writes or response serialization delay successful getPayload admission. `metrics-before.txt`, per-block `metricsN-before.txt`/`metricsN-after.txt`, and `metrics-drained.txt` preserve raw `base_root_experiment` Prometheus lines without distributing deltas over blocks. `result.json` records completion/error, drain status, samples and nearest-rank count/p50/p90/p95/p99 summaries. Partial/error artifacts remain on launch, call, validation, Ctrl-C cancellation and drain failure; nonzero exit means incomplete. Abrupt termination can lose the current in-memory block sample. SIGTERM-specific cleanup is not yet verified.

Phase names are `build_fcu`, `get_payload`, `new_payload`, `canonical_fcu`, `payload_available` and `cycle`. FCU duration alone is admission, not construction time. `payload_available` spans build FCU request to getPayload response. `cycle` spans build request to canonical acceptance. Forced-attribute submission-to-inclusion is not mempool ingress latency. Warmup samples remain raw but are excluded from summaries and accepted-throughput numerators. Throughput divides accepted measured gas and successful included receipt count by measured wall time through completed shutdown/drain, not offered gas/transaction count. Failed or absent samples are never zero-valued successes.

Root metrics include `base_root_experiment_builder_executions`, `validator_executions`, `current_roots_started{source="builder"|"validator"}`, `validator_ready_outputs`, `trusted_insertions`, job counters and root/sorting/wait histograms. Metrics have process scope. A one-process-per-trial policy is required. The harness is included in that process's resource cost.

## External responsibilities and unverified requirements

base-metal owns paired repeated trials, CPU affinity, OS CPU time/RSS/I/O sampling and resource accounting. Queue-depth/cache-hit/miss/persistence-lag/cancellation and paced/missed-cadence measurements are unsupported here. Serialized saturation has no configured cadence and proves no pipeline overlap. A shutdown timeout is incomplete, not evidence that an indivisible provider thread stopped.

Required gates before use: package compile and the colocated `root_bench::tests` on a supported host; actual admitted R/D nonempty activation runs; core semantic/publication/retention regressions; independent first-import metrics; exact state/receipt/nonce/gas parity; immutable baseline parity and source provenance; O/S insertion-path observations; retention stress beyond 128 roots; failure injection for worker panic/drop/timeout and cancellation. No full performance certification is implied by source implementation or unit checks.
