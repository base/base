//! Alt-DA client abstraction for batcher dual-write.
//!
//! The concrete HTTP client lives in the infra `base-alt-da` crate, which the
//! batcher crates must not depend on (see `etc/scripts/ci/check-crate-deps.sh`).
//! The batcher depends on this trait instead; the binary injects a concrete
//! client that adapts `base-alt-da`.

use std::sync::Arc;

use alloy_primitives::Bytes;
use async_trait::async_trait;
use base_protocol::DERIVATION_VERSION_1;

/// Shared handle to an [`AltDaClient`].
pub type DynAltDaClient = Arc<dyn AltDaClient>;

/// Uploads batch bytes to an alt-DA server and returns the server-generated commitment.
///
/// The batcher treats the returned commitment as opaque bytes; validation of the
/// commitment format happens in the concrete client at the network boundary.
#[async_trait]
pub trait AltDaClient: std::fmt::Debug + Send + Sync {
    /// Upload `body`; return the commitment bytes to post on L1.
    async fn put(&self, body: Vec<u8>) -> Result<Vec<u8>, AltDaError>;
}

/// Error returned by an [`AltDaClient`] upload.
#[derive(Debug, thiserror::Error)]
#[error("alt-da client error: {0}")]
pub struct AltDaError(pub String);

/// Encode an alt-DA commitment as L1 calldata: `DERIVATION_VERSION_1` ++ commitment.
pub fn encode_commitment_tx_data(commitment: &[u8]) -> Bytes {
    let mut data = Vec::with_capacity(1 + commitment.len());
    data.push(DERIVATION_VERSION_1);
    data.extend_from_slice(commitment);
    Bytes::from(data)
}
