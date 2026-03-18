//! Error types for attestation proof generation.

use thiserror::Error;

/// Errors that can occur during attestation proof generation.
#[derive(Debug, Error)]
pub enum ProverError {
    /// The underlying attestation verifier rejected the input.
    #[error("verifier error: {0}")]
    Verifier(#[from] base_proof_tee_nitro_verifier::VerifierError),

    /// RISC Zero proving failed (Bonsai, dev-mode, or local).
    #[error("risc0 error: {0}")]
    Risc0(String),

    /// Boundless marketplace interaction failed.
    #[error("boundless error: {0}")]
    Boundless(String),

    /// The guest ELF or image ID is invalid.
    #[error("image ID error: {0}")]
    ImageId(String),
}

/// Convenience result alias for prover operations.
pub type Result<T, E = ProverError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use base_proof_tee_nitro_verifier::VerifierError;
    use rstest::rstest;

    use super::*;

    // ── From conversion ─────────────────────────────────────────────────

    #[rstest]
    fn from_verifier_error() {
        let verifier_err = VerifierError::ContentValidation("bad field".into());
        let prover_err = ProverError::from(verifier_err);

        assert!(matches!(prover_err, ProverError::Verifier(_)));
        assert!(prover_err.to_string().contains("bad field"));
    }

    // ── Display formatting ──────────────────────────────────────────────

    #[rstest]
    #[case::verifier(
        ProverError::Verifier(VerifierError::Cbor("decode failed".into())),
        "verifier error: CBOR error: decode failed"
    )]
    #[case::risc0(
        ProverError::Risc0("segment fault".into()),
        "risc0 error: segment fault"
    )]
    #[case::boundless(
        ProverError::Boundless("timeout".into()),
        "boundless error: timeout"
    )]
    #[case::image_id(
        ProverError::ImageId("not an ELF".into()),
        "image ID error: not an ELF"
    )]
    fn display_formatting(#[case] error: ProverError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    // ── Result alias ────────────────────────────────────────────────────

    #[rstest]
    fn result_alias_defaults_to_prover_error() {
        let err: Result<u32> = Err(ProverError::Risc0("fail".into()));
        assert!(matches!(err, Err(ProverError::Risc0(_))));
    }
}
