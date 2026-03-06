use async_trait::async_trait;
use base_proof_primitives::{ProofBundle, ProofResult};

use crate::TransportError;

/// Result type for proof transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Proves a [`ProofBundle`] and returns the [`ProofResult`].
///
/// Implementations handle the underlying mechanics — in-process call,
/// vsock connection, or remote prover API — so callers remain
/// transport-agnostic. Each `prove()` call is independent and
/// self-contained; concurrency is achieved by the caller spawning
/// multiple `prove()` calls.
#[async_trait]
pub trait ProofTransport: Send + Sync {
    /// Prove a bundle and return the result.
    async fn prove(&self, bundle: &ProofBundle) -> TransportResult<ProofResult>;
}
