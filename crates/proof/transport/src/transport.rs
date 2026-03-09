use async_trait::async_trait;
use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;

use crate::TransportError;

/// Result type for proof transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Sends witness preimages to a prover and returns the [`ProofResult`].
///
/// Implementations handle the underlying mechanics — in-process call,
/// vsock connection, or remote prover API — so callers remain
/// transport-agnostic. Each `prove()` call is independent and
/// self-contained; concurrency is achieved by the caller spawning
/// multiple `prove()` calls.
#[async_trait]
pub trait ProofTransport: Send + Sync {
    /// Send preimages to the prover and return the proof result.
    async fn prove(
        &self,
        preimages: &[(PreimageKey, Vec<u8>)],
    ) -> TransportResult<ProofResult>;
}
