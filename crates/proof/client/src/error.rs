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
