# `base-prover`

TEE prover binary for AWS Nitro Enclaves. Subcommands live under `nitro`:

- **`nitro server`** — Runs the JSON-RPC server on the EC2 host, forwarding proving requests to the enclave over vsock.
- **`nitro enclave`** — Runs the proving process inside the Nitro Enclave, listening on vsock.
- **`nitro local`** *(feature-gated)* — Runs server and enclave in a single process for local development.
