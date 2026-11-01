# `base-proof-tee-nitro-enclave`

Nitro Enclave runtime, types, and proving logic.

This crate contains everything that runs **inside** the Nitro Enclave: the
vsock listener, proof-client pipeline, ECDSA signing, NSM access, and
attestation verification. Core proof types (`Proposal`, `ProofJournal`,
`ProofResult`) are re-exported from `base-proof-primitives`.

It also owns both ends of the Enclave link, the request/response hop between
the host and an enclave: the wire messages (`EnclaveRequest`, `EnclaveResponse`),
length-prefixed framing (`Frame`), the host-side `EnclaveClient`, and the
enclave-side `EnclaveHandler`. `EnclaveClient` reaches an enclave over vsock or
an in-process `Server` over an in-memory stream, so both modes share framing,
request limits, timeouts, and error mapping. `EnclaveLinkError` distinguishes
link failures from Enclave rejections (`EnclaveFailure`). See
[`../GLOSSARY.md`](../GLOSSARY.md).

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
