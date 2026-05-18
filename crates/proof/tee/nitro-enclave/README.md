# `base-proof-tee-nitro-enclave`

Nitro Enclave runtime, types, and proving logic.

This crate contains everything that runs **inside** the Nitro Enclave: the
vsock listener, proof-client pipeline, ECDSA signing, NSM access, and
attestation verification. Core proof types (`Proposal`, `ProofJournal`,
`ProofResult`) are re-exported from `base-proof-primitives`.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
