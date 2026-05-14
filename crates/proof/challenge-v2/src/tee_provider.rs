//! TEE proving service abstraction.
//!
//! [`TeeProofProvider`] is the trait the challenger uses to ask its TEE
//! prover to attest a range of L2 blocks. The fast path for `TeeWrong`
//! disputes (see `crate::prove`) calls this trait first; the result
//! (signed root + signature bytes) is wrapped into a TEE-flavored
//! dispute action when it matches our computed view.

use alloy_primitives::{B256, Bytes};
use async_trait::async_trait;
use thiserror::Error;

/// Output of a TEE proving call: the root the TEE attested to plus
/// the raw 65-byte ECDSA signature over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeeProofResult {
    /// Output root the TEE attested to.
    pub signed_root: B256,
    /// Raw 65-byte ECDSA signature over `signed_root` (`r || s || v`).
    /// The encoder normalizes `v` to 27/28 before submission.
    pub signature_bytes: Bytes,
}

/// Errors returned by [`TeeProofProvider::prove_range`].
#[derive(Debug, Error)]
pub enum TeeProofError {
    /// TEE backend produced an error (transport, signing, attestation, ...).
    #[error("TEE backend error: {0}")]
    Backend(String),
}

/// TEE proving service abstraction.
///
/// Implementations sign an attestation over a range of L2 blocks
/// and return the signed root plus signature bytes. Cheap (seconds,
/// not hours) compared to ZK proving.
#[async_trait]
pub trait TeeProofProvider: Send + Sync + std::fmt::Debug {
    /// Asks the TEE to sign an attestation over the L2 block range
    /// `[start_block, end_block]` rooted at `l1_head`, sampling
    /// intermediate roots at the given checkpoint interval.
    async fn prove_range(
        &self,
        start_block: u64,
        end_block: u64,
        l1_head: B256,
        intermediate_block_interval: u64,
    ) -> Result<TeeProofResult, TeeProofError>;
}
