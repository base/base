# `base-prover-nitro-enclave`

TEE prover enclave binary for AWS Nitro Enclaves.

Runs inside the Nitro Enclave, listening on vsock for proving requests from the
host server. This binary is packaged into the EIF (Enclave Image File) and
executes within the trusted execution environment.
