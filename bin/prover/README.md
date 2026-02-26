# `base-prover`

TEE prover server binary with two subcommands:

- **`nitro`** — Runs the TEE prover enclave server, listening on vsock (Nitro) or HTTP (local dev).
- **`proxy`** — Runs an HTTP-to-vsock bridge for AWS Nitro Enclaves.
