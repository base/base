# `base-proof-transport`

Proof transport abstraction for proof backends.

## Overview

Defines `ProofTransport`, a trait with a single `prove(bundle) -> result` method, so proof
callers remain agnostic to whether the prover runs in-process, in a Nitro Enclave over vsock,
or remotely. `NativeTransport` dispatches calls in-process (useful for testing), while
`VsockTransport` opens a connect-per-request bincode connection over `AF_VSOCK` for AWS Nitro
Enclave deployments.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-transport = { workspace = true }
```

```rust,ignore
use base_proof_transport::{ProofTransport, NativeTransport};

let transport = NativeTransport::new(prover_fn);
let result = transport.prove(bundle).await?;
```

## Transports

| Transport | Purpose |
|-----------|---------|
| `NativeTransport` | In-process channel for testing and single-process provers |
| `VsockTransport` | Connect-per-request bincode over `AF_VSOCK` for Nitro Enclaves (feature `vsock`, unix-only) |

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
