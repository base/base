//! Canonical L1 lookups used to verify that the L1 block a derived candidate's batch landed in is
//! still on the canonical chain before that candidate is finalized.

use std::fmt::Debug;

use async_trait::async_trait;
use base_consensus_derive::ChainProvider;
use base_protocol::BlockInfo;

/// Answers whether a specific L1 block is still canonical.
///
/// Derivation records the L1 inclusion block of every derived candidate, and a candidate may
/// outlive the L1 block its batch landed in: an L1 reorg can replace that block without touching
/// the L2 safe head the reset preserves. Finalization therefore has to confirm that the recorded
/// inclusion block is still canonical before promoting the candidate.
///
/// Implementations are queried for the candidate about to be promoted, on finalization events,
/// rather than once per derived block.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait L1CanonicalChain: Debug + Send + Sync {
    /// Returns whether `block` is the canonical L1 block at its height.
    ///
    /// An error means the lookup itself failed; callers must not treat it as proof either way.
    async fn is_canonical(&self, block: BlockInfo) -> Result<bool, String>;
}

/// An [`L1CanonicalChain`] backed by an L1 chain provider.
#[derive(Debug)]
pub struct ChainProviderCanonicalChain<P> {
    /// The L1 provider, guarded because [`ChainProvider`] lookups take `&mut self`.
    provider: tokio::sync::Mutex<P>,
}

impl<P: Debug> ChainProviderCanonicalChain<P> {
    /// Wraps `provider` so it can answer canonicality questions.
    pub fn new(provider: P) -> Self {
        Self { provider: tokio::sync::Mutex::new(provider) }
    }
}

#[async_trait]
impl<P> L1CanonicalChain for ChainProviderCanonicalChain<P>
where
    P: ChainProvider + Debug + Send,
{
    async fn is_canonical(&self, block: BlockInfo) -> Result<bool, String> {
        let canonical = self
            .provider
            .lock()
            .await
            .block_info_by_number(block.number)
            .await
            .map_err(|error| error.to_string())?;
        Ok(canonical.hash == block.hash)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`ChainProviderCanonicalChain`].

    use alloy_primitives::B256;
    use base_consensus_derive::test_utils::TestChainProvider;
    use base_protocol::BlockInfo;

    use super::{ChainProviderCanonicalChain, L1CanonicalChain};

    fn block(number: u64, hash_byte: u8) -> BlockInfo {
        BlockInfo { number, hash: B256::with_last_byte(hash_byte), ..Default::default() }
    }

    #[tokio::test]
    async fn canonical_block_matches_the_provider() {
        let mut provider = TestChainProvider::default();
        provider.insert_block(7, block(7, 0xaa));
        let chain = ChainProviderCanonicalChain::new(provider);

        assert_eq!(chain.is_canonical(block(7, 0xaa)).await, Ok(true));
    }

    #[tokio::test]
    async fn replaced_block_is_not_canonical() {
        let mut provider = TestChainProvider::default();
        provider.insert_block(7, block(7, 0xbb));
        let chain = ChainProviderCanonicalChain::new(provider);

        assert_eq!(chain.is_canonical(block(7, 0xaa)).await, Ok(false));
    }

    #[tokio::test]
    async fn missing_block_is_a_lookup_error() {
        let chain = ChainProviderCanonicalChain::new(TestChainProvider::default());

        assert!(chain.is_canonical(block(7, 0xaa)).await.is_err());
    }
}
