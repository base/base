# `base-prover`

TEE prover server binary with two subcommands:

- **`serve`** (default) — Runs the TEE prover RPC server, listening on vsock (Nitro) or HTTP (local dev).
- **`proxy`** — Runs an HTTP-to-vsock bridge for AWS Nitro Enclaves.
