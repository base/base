# TDX TEE Prover

Host-side worker support for Intel TDX TEE proof backends.

The crate claims Intel TDX jobs through the prover-service worker API, collects
TDX quotes for signer registration, and signs `ProofJournal` bytes with the TDX
guest signer. Its HTTP server exposes only health and registrar-facing
`enclave_*` methods.

`enclave_signerAttestation` returns encoded `TdxSignerAttestation` payloads:
each payload includes the signer public key, raw TDX quote, quote timestamp,
and optional registrar nonce. When supplied, the nonce is bound with the quote
timestamp in `TDREPORT.REPORTDATA`. TDX attestations reject `user_data`.
