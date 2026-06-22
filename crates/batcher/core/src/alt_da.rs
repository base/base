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

/// Generic commitment type byte (`0x01`).
pub const GENERIC_COMMITMENT_TYPE: u8 = 0x01;
/// Generic commitment sentinel byte (`0xff`).
pub const GENERIC_COMMITMENT_SENTINEL: u8 = 0xff;
/// Encoded generic commitment length in bytes.
pub const GENERIC_COMMITMENT_LEN: usize = 34;

/// Fixed-size generic commitment returned by an alt-DA server PUT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenericCommitment([u8; GENERIC_COMMITMENT_LEN]);

impl GenericCommitment {
    /// Wrap raw commitment bytes returned by an alt-DA server PUT.
    pub const fn new(bytes: [u8; GENERIC_COMMITMENT_LEN]) -> Self {
        Self(bytes)
    }

    /// Raw commitment bytes.
    pub const fn as_bytes(&self) -> &[u8; GENERIC_COMMITMENT_LEN] {
        &self.0
    }

    /// Encode L1 calldata for this commitment: `DERIVATION_VERSION_1` ++ commitment bytes.
    pub fn encode_tx_data(&self) -> Bytes {
        let mut data = Vec::with_capacity(1 + GENERIC_COMMITMENT_LEN);
        data.push(DERIVATION_VERSION_1);
        data.extend_from_slice(&self.0);
        Bytes::from(data)
    }
}

/// Shared handle to an [`AltDaClient`].
pub type DynAltDaClient = Arc<dyn AltDaClient>;

/// Uploads batch bytes to an alt-DA server and returns the server-generated commitment.
///
/// Validation of the commitment format happens in the concrete client at the network
/// boundary; the batcher only encodes the typed [`GenericCommitment`] for L1.
#[async_trait]
pub trait AltDaClient: std::fmt::Debug + Send + Sync {
    /// Upload `body`; return the commitment bytes to post on L1.
    async fn put(&self, body: Bytes) -> Result<GenericCommitment, AltDaError>;
}

/// Error returned by an [`AltDaClient`] upload.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct AltDaError(pub Box<dyn std::error::Error + Send + Sync>);
