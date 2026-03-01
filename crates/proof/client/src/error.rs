//! Error types for the proof client.

use base_consensus_derive::PipelineErrorKind;
use base_proof::OracleProviderError;
use base_proof_preimage::errors::PreimageOracleError;
use thiserror::Error;

/// An error that can occur in the fault proof client.
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

    /// Trace extension detected - agreed and claimed output roots match.
    #[error("trace extension detected")]
    TraceExtension,
}
