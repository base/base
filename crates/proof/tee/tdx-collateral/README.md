# base-proof-tee-tdx-collateral

Host-side Intel TDX collateral hydration for attestation proof generation.

This crate fetches Intel PCS collateral and CRLs, validates the host-side
collateral bundle, and returns explicit verifier input components for TDX
registration proofs.

It intentionally stays separate from the registrar orchestration crate and the
TDX attestation prover backend.
