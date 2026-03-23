# `base-prover-nitro`

TEE prover binary for AWS Nitro Enclaves.

## Subcommands

- **`server`** — Runs the JSON-RPC server on the EC2 host, forwarding proving requests to the enclave over vsock.
- **`enclave`** — Runs the proving process inside the Nitro Enclave, listening on vsock.
- **`local`** *(feature-gated)* — Runs server and enclave in a single process for local development.
