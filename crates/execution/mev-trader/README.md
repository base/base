# Base MEV Trader

`base-mev-trader` is the read-only Phase A measurement engine for in-node MEV analysis.
It captures opaque pending-state snapshots, validates decoded victim frames against that
snapshot, executes only against hash-pinned state, and emits measurement data rather than
transactions.

The crate intentionally contains no network transport, signer, transaction submission, or
transaction-pool integration. Runtime installation is separately gated by the execution CLI.

## Measurement authority

`ProcessedFrame` is opaque evidence that the production frame processor completed its
authority, decode, execution, delta-guard, commit, materialization, lifecycle, and final-authority
checks. The pairwise measurement selector accepts that proof; `MeasurementContext` is exposed
only for immutable inspection and is not plan-construction authority.

`BackrunPlan` and its canonical bytes are unsigned measurement evidence. The crate provides no
transaction or envelope conversion, signing, submission, forwarding, or outbound transport for
that evidence.
## Persisted admission evidence

The admission exporter is a sibling schema and writer, not an extension of the feature-gated edge
economics writer. The execution CLI enables it from Blink configuration independently of T4a/T4b/T4d
shadow conjunctions when all three values are present:
`MEV_TRADER_T4A_ADMISSION_OUTPUT_ROOT`, `MEV_TRADER_T4A_ADMISSION_RUN_ID`, and
`MEV_TRADER_T4A_ADMISSION_BOOT_ID`. Setting `OUTPUT_ROOT` without either identifier is a fail-closed
node startup error. The output root must already exist as a private non-symlink directory; creating
or provisioning the production state root remains a separate node-operation review. Records never
contain raw transaction bytes, credentials, or state values.
Footer `valid` deliberately combines two scopes: `exporterQueueDropped` is cumulative for the
entire run because any queue loss invalidates the campaign, while sequence, contract, terminal, and
reconciliation counters reset for each segment. Therefore `valid:false` can mean either a run-wide
queue-loss failure or a segment-local failure; consumers must inspect the footer fields to distinguish
the scope and must not interpret `valid` as segment-local health alone.

This Phase A build has no downstream total-cost/EV callback. Consequently `gross_nonpositive`,
`ev_nonpositive`, `ev_positive`, non-null `grossShortfallToPositiveWei`, and downstream
T4b/T4d/handoff/T4e terminal reasons are schema-reserved but runtime-unreachable; completing that
event-contract economics contract is explicitly deferred to the authority-integration phase.
`cohortVersion` remains null until cohort configuration is installed. `dirtyPoolCount` and
`inUniverseDirty` remain null before delta analysis; once present, `inUniverseDirty` is derived from
the universe-validated dirty-pool set rather than an independent estimate. Generation overflow has
no frame sequence to persist, and read-plan-storage identity awaits a typed materializer error.

## Offline latency evidence

The ignored release fixture is deterministic and local-only. It measures, from
`VictimFrame.received_at`, adjacent snapshot discovery, prepared-pool canonicalization, successful
production processing, two-hop discovery plus proof-bound selection, and one final evidence
encoding. Validation after the final encode is an untimed correctness check. The fixture uses ten
warmups followed by one hundred sequential timed samples with no concurrent load, sample
curation, relabeling, or retry-to-pass behavior.

The resulting JSON is measurement evidence only. Phase-B signing latency, sequencer latency, and
attribution remain exactly `UNKNOWN`; the local fixture does not infer them.
