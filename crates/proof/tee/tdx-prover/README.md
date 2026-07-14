# TDX TEE Prover

Host-side worker support for Intel TDX TEE proof backends.

The crate claims Intel TDX jobs through the prover-service worker API, requests
Confidential Space attestation tokens for signer registration, and signs
`ProofJournal` bytes with the TDX guest signer. Its HTTP server exposes only
health and registrar-facing `enclave_*` methods.

TEE proof journals use the OCI manifest digest in the Confidential Space token.
They do not use a self-reported binary hash or a raw TDX quote.

`enclave_signerAttestation` returns encoded `TdxSignerAttestation` payloads:
each payload includes the signer public key, Google Cloud Attestation PKI token,
L1 chain ID, registry address, and registrar nonce. The token nonce binds the
signer and registration context. TDX attestations reject `user_data`.
