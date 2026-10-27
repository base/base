//! Canonical L1 lookups used to verify that the L1 block a derived candidate's batch landed in is
//! still on the canonical chain before that candidate is finalized.

use std::fmt::Debug;

use async_trait::async_trait;
use base_consensus_derive::ChainProvider;
use base_consensus_providers::AlloyChainProviderError;
use base_protocol::BlockInfo;
use thiserror::Error;

/// The reason an L1 canonicity lookup could not be answered.
///
/// Callers treat every variant as "not verified", because none of them proves anything about
/// canonicality. The kind still matters: a block the provider does not know says nothing at all and
/// is expected at heights above a lagging node's tip, while a transport failure or a timeout leaves
/// the question open. Keeping them apart also keeps the provider's own error as the source instead
/// of flattening it into a string.
#[derive(Debug, Error)]
pub enum L1CanonicalChainError {
    /// The provider does not know the block at that height.
    #[error("L1 block {number} is unknown to the L1 provider")]
    BlockNotFound {
        /// The height that was asked for.
        number: u64,
    },
    /// The lookup did not answer within its deadline.
    #[error("L1 canonicity lookup for block {number} timed out")]
    Timeout {
        /// The height that was asked for.
        number: u64,
    },
    /// The provider request failed.
    #[error("L1 canonicity lookup for block {number} failed")]
    Provider {
        /// The height that was asked for.
        number: u64,
        /// The provider's own error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl L1CanonicalChainError {
    /// Classifies a provider error for the block at `number`, keeping it as the source.
    pub fn from_provider(
        number: u64,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        let source: Box<dyn std::error::Error + Send + Sync> = Box::new(source);
        match source.downcast_ref::<AlloyChainProviderError>() {
            Some(AlloyChainProviderError::BlockNotFound(_)) => Self::BlockNotFound { number },
            _ => Self::Provider { number, source },
        }
    }
}

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
    async fn is_canonical(&self, block: BlockInfo) -> Result<bool, L1CanonicalChainError>;
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
    P::Error: std::error::Error + Send + Sync + 'static,
{
    async fn is_canonical(&self, block: BlockInfo) -> Result<bool, L1CanonicalChainError> {
        let canonical = self
            .provider
            .lock()
            .await
            .block_info_by_number(block.number)
            .await
            .map_err(|error| L1CanonicalChainError::from_provider(block.number, error))?;
        Ok(canonical.hash == block.hash)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`ChainProviderCanonicalChain`].

    use alloy_primitives::B256;
    use base_consensus_derive::test_utils::TestChainProvider;
    use base_consensus_providers::AlloyChainProviderError;
    use base_protocol::BlockInfo;

    use super::{ChainProviderCanonicalChain, L1CanonicalChain, L1CanonicalChainError};

    fn block(number: u64, hash_byte: u8) -> BlockInfo {
        BlockInfo { number, hash: B256::with_last_byte(hash_byte), ..Default::default() }
    }

    #[tokio::test]
    async fn canonical_block_matches_the_provider() {
        let mut provider = TestChainProvider::default();
        provider.insert_block(7, block(7, 0xaa));
        let chain = ChainProviderCanonicalChain::new(provider);

        assert!(chain.is_canonical(block(7, 0xaa)).await.expect("the block is known"));
    }

    #[tokio::test]
    async fn replaced_block_is_not_canonical() {
        let mut provider = TestChainProvider::default();
        provider.insert_block(7, block(7, 0xbb));
        let chain = ChainProviderCanonicalChain::new(provider);

        assert!(!chain.is_canonical(block(7, 0xaa)).await.expect("the block is known"));
    }

    #[tokio::test]
    async fn missing_block_is_a_lookup_error() {
        let chain = ChainProviderCanonicalChain::new(TestChainProvider::default());

        let error = chain.is_canonical(block(7, 0xaa)).await.expect_err("the block is unknown");
        // The provider's answer is kept as the source instead of being flattened into a string.
        assert!(matches!(error, L1CanonicalChainError::Provider { number: 7, .. }));
    }

    #[test]
    fn an_unknown_block_keeps_its_own_kind() {
        let error = L1CanonicalChainError::from_provider(
            7,
            AlloyChainProviderError::BlockNotFound(7u64.into()),
        );

        assert!(matches!(error, L1CanonicalChainError::BlockNotFound { number: 7 }));
    }
}
