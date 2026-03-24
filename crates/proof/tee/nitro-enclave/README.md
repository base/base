# `base-proof-tee-nitro-enclave`

Nitro Enclave runtime, types, and proving logic.

This crate contains everything that runs **inside** the Nitro Enclave: the
vsock listener, proof-client pipeline, ECDSA signing, NSM access, and
attestation verification. It also defines the enclave-native proof types
(`TeeProofResult`, `ProofJournal`, `Proposal`) so that host-side type changes
(e.g. adding fields to `ProofRequest`) do not alter the enclave binary and
therefore do not change the PCR0 measurement.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
