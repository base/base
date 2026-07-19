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

## Offline latency evidence

The ignored release fixture is deterministic and local-only. It measures, from
`VictimFrame.received_at`, adjacent snapshot discovery, prepared-pool canonicalization, successful
production processing, two-hop discovery plus proof-bound selection, and one final evidence
encoding. Validation after the final encode is an untimed correctness check. The fixture uses ten
warmups followed by one hundred sequential timed samples with no concurrent load, sample
curation, relabeling, or retry-to-pass behavior.

The resulting JSON is measurement evidence only. Phase-B signing latency, sequencer latency, and
attribution remain exactly `UNKNOWN`; the local fixture does not infer them.
