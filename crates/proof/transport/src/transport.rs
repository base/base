use async_trait::async_trait;
use base_proof_primitives::{ProofResult, WitnessBundle};

use crate::TransportError;

/// Result type for witness transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Abstracts how witness bundles reach proof backends and how results come
/// back.
///
/// Implementations handle the underlying channel — in-process, `AF_VSOCK`, or
/// ZK guest stdin — so callers remain transport-agnostic.
#[async_trait]
pub trait WitnessTransport: Send + Sync {
    /// Send a witness bundle to the proof backend.
    async fn send_witness(&self, bundle: WitnessBundle) -> TransportResult<()>;

    /// Receive a proof result from the backend.
    async fn recv_result(&self) -> TransportResult<ProofResult>;
}
