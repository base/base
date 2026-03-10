# base-proof-tee-nitro

Enclave-side module for AWS Nitro Enclave proof generation.

Receives a `ProofBundle` over vsock, runs the proof-client pipeline,
signs the result, and returns a `ProofResult`.
