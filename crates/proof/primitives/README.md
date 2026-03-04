# base-proof-primitives

Shared primitive types and traits for the Base proof system.

This crate provides the core protocol types used across TEE, ZK, and FPVM proof
backends:

- **`WitnessBundle`** — Wire format carrying a set of preimage key-value pairs.
- **`ProofClaim`** — The claim being proven: L2 block number, output root, and
  L1 head.
- **`ProofEvidence`** — Backend-specific evidence (TEE attestation or ZK proof).
- **`ProofResult`** — A claim paired with its evidence.
