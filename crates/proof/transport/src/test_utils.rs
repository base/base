use async_trait::async_trait;
use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;

use crate::{ProofTransport, TransportResult};

/// In-process proof transport that delegates to a handler function.
pub struct NativeTransport<F> {
    handler: F,
}

impl<F> NativeTransport<F>
where
    F: Fn(&[(PreimageKey, Vec<u8>)]) -> ProofResult + Send + Sync,
{
    /// Create a new transport that delegates `prove` calls to `handler`.
    pub const fn new(handler: F) -> Self {
        Self { handler }
    }
}

impl<F> std::fmt::Debug for NativeTransport<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeTransport").finish_non_exhaustive()
    }
}

#[async_trait]
impl<F> ProofTransport for NativeTransport<F>
where
    F: Fn(&[(PreimageKey, Vec<u8>)]) -> ProofResult + Send + Sync,
{
    async fn prove(
        &self,
        preimages: &[(PreimageKey, Vec<u8>)],
    ) -> TransportResult<ProofResult> {
        Ok((self.handler)(preimages))
    }
}
