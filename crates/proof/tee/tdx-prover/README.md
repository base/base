# TDX TEE Prover

Host-side worker support for Intel TDX TEE proof backends.

The crate claims Intel TDX jobs through the prover-service worker API, collects
TDX quotes for signer registration, and signs `ProofJournal` bytes with the TDX
guest signer. Its HTTP server exposes only health and registrar-facing
`enclave_*` methods.

TEE proof journals use the configured CI-derived OCI manifest digest. They do
not derive their image hash from a TDX quote.

`enclave_signerAttestation` returns encoded `TdxSignerAttestation` payloads:
each payload includes the signer public key, raw TDX quote, quote timestamp,
workload digest, L1 chain ID, registry address, and optional registrar nonce.
Those values are bound in `TDREPORT.REPORTDATA`. TDX attestations reject
`user_data`.
