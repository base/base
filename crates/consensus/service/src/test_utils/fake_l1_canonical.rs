//! An [`L1CanonicalChain`] for the in-memory actor harness.

use async_trait::async_trait;
use base_protocol::BlockInfo;

use crate::{L1CanonicalChain, L1CanonicalChainError};

/// Reports every L1 block as canonical.
///
/// The in-memory harness never reorgs an L1 block that derivation already built on, so the
/// canonicality check cannot drop a candidate there. Tests that need a replaced inclusion block use
/// a mock instead.
#[derive(Debug, Default)]
pub struct AlwaysCanonical;

#[async_trait]
impl L1CanonicalChain for AlwaysCanonical {
    async fn is_canonical(&self, _block: BlockInfo) -> Result<bool, L1CanonicalChainError> {
        Ok(true)
    }
}
