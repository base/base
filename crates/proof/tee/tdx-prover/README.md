# TDX TEE Prover

Host-side worker support for Intel TDX TEE proof backends.

The crate claims Intel TDX jobs through the prover-service worker API, collects
TDX quotes for signer registration, and signs `ProofJournal` bytes with the TDX
guest signer. Its HTTP server exposes only health and registrar-facing
`enclave_*` methods.

`enclave_signerAttestation` returns encoded `TdxSignerAttestation` payloads:
each payload includes the signer public key, the raw TDX quote, and the quote
timestamp committed into `TDREPORT.REPORTDATA`. TDX attestations currently
reject `user_data` and `nonce` parameters because the runtime does not bind
those challenge fields into report data.
