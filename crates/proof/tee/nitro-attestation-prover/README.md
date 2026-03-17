# base-proof-tee-nitro-attestation-prover

ZK attestation prover for AWS Nitro Enclave attestation documents.

Wraps [`base-proof-tee-nitro-verifier`] verification logic inside a RISC Zero
ZK guest program and provides two proving backends for generating on-chain
verifiable proofs:

- **`DirectProver`** — uses `risc0_zkvm::default_prover()` which routes to
  Bonsai remote proving (`BONSAI_API_KEY`), dev-mode (`RISC0_DEV_MODE=1`),
  or local CPU proving as a fallback.
- **`BoundlessProver`** — submits proof requests to the Boundless marketplace
  for decentralised proving.

The guest ELF is loaded at runtime (from disk or IPFS) rather than embedded at
compile time, so the risc0 toolchain is not required for building this crate.

## Modules

- **`error`** — [`ProverError`] enum covering verification, risc0, and
  Boundless failures.
- **`types`** — [`AttestationProof`] output type and
  [`AttestationProofProvider`] trait.
- **`direct`** — [`DirectProver`] implementation using `default_prover()`.
- **`boundless`** — [`BoundlessProver`] implementation using the Boundless
  marketplace.
