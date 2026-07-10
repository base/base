# base-proof-tee-attestation

Shared TEE attestation proof types for signer registration.

This crate defines the platform-neutral proof model used by registrar code and
TEE-specific attestation provers. Nitro and TDX provers both return a
`TeeAttestationProof` tagged with its `TeeAttestationKind`, allowing the
registrar loop to stay independent from a specific attestation backend.
