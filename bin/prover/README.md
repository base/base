# `base-prover`

TEE prover binary for AWS Nitro Enclaves with two subcommands:

- **`server`** — Runs the JSON-RPC server on the EC2 host, forwarding proving requests to the enclave over vsock.
- **`enclave`** — Runs the proving process inside the Nitro Enclave, listening on vsock.
