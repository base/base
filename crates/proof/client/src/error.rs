use base_consensus_derive::PipelineErrorKind;
use base_proof::OracleProviderError;
use base_proof_preimage::errors::PreimageOracleError;
use thiserror::Error;

/// An error that can occur in the proof client.
#[derive(Error, Debug)]
pub enum FaultProofProgramError {
    /// Preimage oracle error.
    #[error(transparent)]
    OraclePreimage(#[from] PreimageOracleError),
    /// Oracle provider error.
    #[error(transparent)]
    OracleProvider(#[from] OracleProviderError),
    /// Pipeline error.
    #[error(transparent)]
    Pipeline(#[from] PipelineErrorKind),
    /// Driver execution error.
    #[error(transparent)]
    Driver(#[from] base_proof_driver::DriverError<base_proof_executor::ExecutorError>),
    /// The computed output root does not match the claimed output root.
    #[error("invalid claim: computed {computed}, claimed {claimed}")]
    InvalidClaim {
        /// The computed output root.
        computed: alloy_primitives::B256,
        /// The claimed output root.
        claimed: alloy_primitives::B256,
    },
    /// Derivation ended at a different L2 block than the claim.
    #[error("invalid claim block: derived {derived}, claimed {claimed}")]
    InvalidClaimBlock {
        /// The final derived L2 block number.
        derived: u64,
        /// The claimed L2 block number.
        claimed: u64,
    },
    /// Trace extension detected — agreed and claimed output roots match.
    #[error("trace extension detected")]
    TraceExtension,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::FaultProofProgramError;

    #[test]
    fn test_invalid_claim_display() {
        let computed = B256::from([1u8; 32]);
        let claimed = B256::from([2u8; 32]);
        let err = FaultProofProgramError::InvalidClaim { computed, claimed };
        let msg = alloc::format!("{err}");
        assert!(msg.contains("invalid claim"));
        assert!(msg.contains(&alloc::format!("{computed}")));
        assert!(msg.contains(&alloc::format!("{claimed}")));
    }

    #[test]
    fn test_invalid_claim_block_display() {
        let err = FaultProofProgramError::InvalidClaimBlock { derived: 6, claimed: 7 };
        assert_eq!(alloc::format!("{err}"), "invalid claim block: derived 6, claimed 7");
    }

    #[test]
    fn test_trace_extension_display() {
        let err = FaultProofProgramError::TraceExtension;
        assert_eq!(alloc::format!("{err}"), "trace extension detected");
    }
}
