//! Trait abstracting TEE proof generation for testability.

use async_trait::async_trait;
use base_proof_primitives::{ProofRequest, ProofResult};

use crate::ClientError;

/// Abstraction over a TEE prover that accepts a [`ProofRequest`] and returns a
/// [`ProofResult`].
///
/// The canonical implementation delegates to
/// [`EnclaveClient::prove`](crate::EnclaveClient::prove), but the trait allows
/// callers (e.g. the challenger) to swap in a mock for testing without needing
/// real RPC servers.
#[async_trait]
pub trait TeeProver: Send + Sync {
    /// Generate a complete TEE proof for the given request.
    async fn prove(&self, request: ProofRequest) -> Result<ProofResult, ClientError>;
}

#[async_trait]
impl TeeProver for crate::EnclaveClient {
    async fn prove(&self, request: ProofRequest) -> Result<ProofResult, ClientError> {
        self.prove(request).await
    }
}
