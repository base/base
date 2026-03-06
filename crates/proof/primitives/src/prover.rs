use alloc::boxed::Box;
use core::fmt::Debug;

use base_proof_preimage::WitnessOracle;

use crate::ProofResult;

/// A proof backend that owns its witness oracle type and proving logic.
///
/// Each backend (TEE, SP1, RISC Zero) implements this with its own
/// oracle storage layout and proving strategy.
///
/// The oracle type must implement [`WitnessOracle`] for the capture path
/// (receiving preimages during witness generation) and must be `Send + Sync`
/// so it can be shared with the host's preimage server.
#[async_trait::async_trait]
pub trait ProverBackend: Send + Sync {
    /// The backend-specific oracle that receives preimages during witness generation.
    type Oracle: WitnessOracle + Debug + Send + Sync + 'static;

    /// The error type returned by this backend's proving operations.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Create a fresh oracle instance for a single proof's witness generation.
    fn create_oracle(&self) -> Self::Oracle;

    /// Execute the proof using the populated witness oracle.
    ///
    /// Called after [`Host::build_witness`] has filled the oracle with preimages
    /// and called `finalize()`.
    async fn prove(&self, witness: Self::Oracle) -> Result<ProofResult, Self::Error>;
}
