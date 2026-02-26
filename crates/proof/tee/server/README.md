# `base-enclave-server`

Enclave server implementation.

This crate provides the server-side enclave logic for AWS Nitro Enclaves,
including:

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
