# `base-proof-tee-nitro-host`

Host-side TEE proving backend for AWS Nitro Enclaves.

## Overview

Provides the host-side (parent instance) components of the TEE proving pipeline.
Depends on `base-proof-tee-nitro-enclave` for enclave types and the Enclave link
client (`EnclaveClient`).

In production the host reaches each enclave over vsock. In local development
mode the enclave server runs in-process behind the same Enclave link, without
vsock or NSM hardware.

## Modules

| Module | Description |
|---|---|
| `host` | `NitroHost` — wires shared worker discovery/submit onto the Nitro enclave pool |
| `server` | `NitroProverServer` — JSON-RPC server (`prover_*`, `enclave_*`) |
| `pool` | `NitroEnclavePool` — reusable enclave selection, concurrency, and registration guard |
| `proof_generator` | Claimed-job handler that proves via the enclave pool and submits through `base-proof-worker` |
| `backend` | `NitroBackend` — `ProverBackend` impl dispatching to the enclave through `EnclaveClient` |

## Usage

```toml
[dependencies]
base-proof-tee-nitro-host = { workspace = true }
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
