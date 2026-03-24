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
| `server` | `NitroProverServer` — JSON-RPC server (`prover_*`, `enclave_*`) |
| `backend` | `NitroBackend` — `ProverBackend` impl dispatching to enclave via transport |
| `transport` | `NitroTransport` — vsock (production) or in-process (local dev) |
| `vsock` | *(Linux-only)* `VsockTransport` — frame-based vsock communication with timeouts |
| `convert` | Type conversions between enclave-native and host-side proof types |

## Usage

```toml
[dependencies]
base-proof-tee-nitro-host = { workspace = true }
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
