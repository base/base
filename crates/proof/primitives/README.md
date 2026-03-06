# base-proof-primitives

Shared primitive types and traits for the Base proof system.

This crate provides the core protocol types used across TEE, ZK, and FPVM proof
backends:

- **`Proposal`** — An output root proposal with its ECDSA signature and L1/L2 context.
- **`ProofBundle`** — Request plus preimage key-value pairs sent to a prover.
- **`ProofClaim`** — The claim being proven: L2 block number, output root, and
  L1 head.
- **`ProofEvidence`** — Backend-specific evidence (TEE attestation or ZK proof).
- **`ProofResult`** — A claim paired with its evidence.
