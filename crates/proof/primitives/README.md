# base-proof-primitives

Shared primitive types and traits for the Base proof system.

This crate provides the core protocol types used across TEE, ZK, and FPVM proof
backends:

- **`Proposal`** — An output root proposal with its ECDSA signature and L1/L2 context.
  Fields are untrusted until validated by the consumer.
- **`ProofBundle`** — Request plus preimage key-value pairs sent to a prover.
- **`ProofClaim`** — The claim being proven: an aggregated `Proposal` covering the
  entire block range and the individual per-block `Proposal`s that were aggregated.
  Use `ProofClaim::validate()` to check structural invariants (non-empty proposals
  within `MAX_PROPOSALS`, correct signature lengths) before trusting data from
  untrusted sources.
- **`ProofClaimError`** — Validation errors returned by `ProofClaim::validate()`.
- **`ProofEvidence`** — Backend-specific evidence (TEE attestation or ZK proof).
- **`ProofResult`** — A claim paired with its evidence.
