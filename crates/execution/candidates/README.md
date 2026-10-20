# base-execution-candidates

Domain types, validation and the in-memory group ledger for builder-side candidate selection
("just-in-time routing") behind `base_sendSwapTransaction`, the Swap Lane submission endpoint.

A sender submits a signed default transaction and, optionally, signed alternatives that share
its nonce, plus a signed EIP-712 envelope (`CandidateSet`) that states how to choose between
them. The default transaction is the envelope's representative; together they are the group's
siblings. At the exact block position where the group executes, the builder trials the
siblings without committing them, picks the winner by the sender's rule, and commits only the
winner, or applies the signed fallback policy.

This crate holds the builder-independent parts:

- `CandidateEnvelope` and the `CandidateSet`/`SelectionRule` EIP-712 types. The envelope's
  signing hash is the group identity (`groupHash`). The sender signs no expiry: the node gives
  each admitted group a lifetime from its configuration.
- `CandidateValidator`: bounds, decode, sibling uniformity, envelope and signature checks, with
  the stable error labels of `CandidateError`. A default transaction submitted alone gets the
  same per-transaction size, type and gas caps.
- `GroupDescriptor` and `GroupBudget`: a verified group's identity, members, node-assigned
  expiry block and work budget, without its signature or transaction bytes.
- `GroupLedger`: one atomic record per group descriptor that keeps attempts, work budgets and
  sibling tombstones independent of pool membership. It returns `LedgerError`; ingress maps it
  to the stable labels.
- `BlockWindow`: a duration converted to an inclusive block range, rounding up.
- `CandidateAccessPolicy` and the allowlisted zero-charge `StaticAccessPolicy`. A principal is
  one identity with a separate entitlement and work budget per `Capability`.
- `GroupLifecycle`: the normative turn precedence (expiry, ledger, revocation, kill switch,
  budgets, per-build limits, evaluation, fallback).
- `WinnerSelection` and `ScoreRead`: score-read context v1 decoding, the overflow-free delta,
  and the winner rule (highest delta, then lowest gas, then lowest index).
- `CandidateRuntime`: the ledger, policy, kill switch and limits shared by ingress, the pool
  and the builder.

Everything is off by default. The ledger, tombstones and principal usage live only in memory,
so enabling ingress requires `--txpool.candidates.ephemeral-ledger` and is limited to devnets
and tests. Production use also requires the deferred forwarding, privacy and recovery work
described in the design.
