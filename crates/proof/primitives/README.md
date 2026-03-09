# `base-proof-primitives`

Shared primitive types and traits for the Base proof system.

## Overview

Provides the core protocol types used across TEE, ZK, and FPVM proof backends. Defines the
wire format for proof requests and results that flow between the proposer, enclave, and
on-chain verifiers:

- **`Proposal`** — An output root proposal with its ECDSA signature and L1/L2 context.
- **`ProofBundle`** — Request plus preimage key-value pairs sent to a prover.
- **`ProofClaim`** — The claim being proven: an aggregated `Proposal` covering the
  entire block range and the individual per-block `Proposal`s that were aggregated.
- **`ProofEvidence`** — Backend-specific evidence (TEE attestation or ZK proof).
- **`ProofResult`** — A claim paired with its evidence.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-primitives = { workspace = true }
```

```rust,ignore
use base_proof_primitives::{Proposal, ProofBundle, ProofResult};
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
