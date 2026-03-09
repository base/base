# `base-enclave-server`

Enclave server implementation.

## Overview

Implements the server-side TEE enclave logic for AWS Nitro Enclaves. Handles session management
and random number generation via the Nitro Secure Module (`NSM Interface`), AWS attestation
document verification, RSA-4096 and ECDSA secp256k1 key operations, and a JSON-RPC server
(jsonrpsee) exposed over vsock or HTTP transports.

Key components:

- **NSM Interface**: Session management and random number generation using
  the Nitro Secure Module
- **Attestation**: Verification of AWS Nitro Enclave attestation documents
- **Cryptography**: RSA-4096 and ECDSA secp256k1 key operations
- **Server**: Main server struct for key management and attestation
- **RPC**: JSON-RPC server interface using jsonrpsee
- **Transport**: Vsock and HTTP transport configuration

## Local Mode

When running outside of a Nitro Enclave (or on non-Linux platforms),
the server operates in "local mode":
- No NSM operations (attestation unavailable)
- Uses OS random number generator
- Optionally loads signer key from `OP_ENCLAVE_SIGNER_KEY` environment variable

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-enclave-server = { workspace = true }
```

```rust,ignore
use base_enclave_server::{EnclaveServer, TransportConfig};

let server = EnclaveServer::new(TransportConfig::Vsock { port: 5000 });
server.run().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
