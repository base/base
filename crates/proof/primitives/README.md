# `base-proof-primitives`

Shared primitive types and traits for the Base proof system.

## Overview

Provides the core protocol types used across TEE, ZK, and FPVM proof backends. Defines the
wire format for proof requests and results that flow between the proposer, enclave, and
on-chain verifiers:

- **`Proposal`** — An output root proposal with its ECDSA signature and L1/L2 context.
- **`ProofBundle`** — Request plus preimage key-value pairs sent to a prover.
- **`ProofResult`** — The output of a proof computation, with one variant per backend:
  - `Tee` — aggregated and per-block `Proposal`s.
  - `Zk` — opaque proof bytes.

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
