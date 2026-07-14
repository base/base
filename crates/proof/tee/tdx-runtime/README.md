# base-proof-tee-tdx-runtime

Runtime helpers for Intel TDX signer identity and Confidential Space token collection.

The crate owns secp256k1 signer key generation inside the guest, derives the
uncompressed signer public key and Ethereum address, and requests Google Cloud
Attestation PKI tokens from the Confidential Space launcher through its Unix
socket.

The token claims identify the production Confidential Space launcher, Intel TDX
VM, and OCI workload image. Local tests use deterministic token fixtures.

The registrar challenge is hashed with the signer public key, L1 chain ID, and
`TEEProverRegistry` address before it is supplied as a token nonce.
