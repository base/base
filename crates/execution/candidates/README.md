# base-execution-candidates

Builder-side candidate selection for `base_sendRawTransactionCandidates`.

A sender submits several signed sibling transactions that share one nonce, plus a signed
EIP-712 envelope (`CandidateSet`) that states how to choose between them. The builder
simulates the siblings at the exact block position where the group executes, picks the
winner by the sender's rule, and commits only the winner.

This crate holds the builder-independent parts:

- `CandidateEnvelope` and the `CandidateSet`/`SelectionRule` EIP-712 types. The envelope's
  signing hash is the group identity (`groupHash`).
- `CandidateValidator`: bounds, decode, sibling uniformity, envelope, expiry window and
  signature checks, with the stable error labels of `CandidateError`.
- `GroupLedger`: one atomic record per group that keeps attempts, work budgets and sibling
  tombstones independent of pool membership.
- `CandidateAccessPolicy` and the allowlisted zero-charge `StaticAccessPolicy`.
- `GroupLifecycle`: the normative turn precedence (expiry, ledger, revocation, kill switch,
  budgets, per-build limits, evaluation, fallback).
- `WinnerSelection` and `ScoreRead`: score-read context v1 decoding, the overflow-free delta,
  and the winner rule (highest delta, then lowest gas, then lowest index).
- `CandidateRuntime`: the ledger, policy, kill switch and limits shared by ingress, the pool
  and the builder.

Everything is off by default. Enabling candidate ingress in production also requires the
deferred forwarding, privacy and recovery work described in the design.
