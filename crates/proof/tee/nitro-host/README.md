# `base-proof-tee-nitro-host`

Host-side TEE proving backend for AWS Nitro Enclaves.

## Overview

Provides the host-side (parent instance) components of the TEE proving pipeline.
Depends on `base-proof-tee-nitro-enclave` for enclave types and protocol definitions.

In production the host forwards preimages over vsock to the enclave. In local
development mode the enclave server runs in-process without vsock or NSM hardware.

## Modules

| Module | Description |
|---|---|
| `host` | `NitroHost` — wires shared worker discovery/submit onto the Nitro enclave pool |
| `server` | `NitroProverServer` — JSON-RPC server (`prover_*`, `enclave_*`) |
| `pool` | `NitroEnclavePool` — reusable enclave selection, concurrency, and registration guard |
| `proof_generator` | Claimed-job handler that proves via the enclave pool and submits through `base-proof-worker` |
| `backend` | `NitroBackend` — `ProverBackend` impl dispatching to enclave via transport |
| `transport` | `NitroTransport` — vsock (production) or in-process (local dev) |
| `vsock` | *(Linux-only)* `VsockTransport` — frame-based vsock communication with timeouts |

## Usage

```toml
[dependencies]
base-proof-tee-nitro-host = { workspace = true }
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
