//! Shared sealed-or-recovered block container.

use alloc::sync::Arc;
use core::ops::Deref;

use crate::block::{RecoveredBlock, SealedBlock, error::SealedBlockRecoveryError};

/// A block that is either sealed or sealed with recovered transaction senders.
///
/// This is useful for APIs that must accept ordinary sealed blocks, but can skip sender recovery
/// when the caller already has a [`RecoveredBlock`].
#[derive(Debug, Clone)]
pub enum SealedOrRecoveredBlock {
    /// A sealed block without recovered senders.
    Sealed(Arc<SealedBlock>),
    /// A sealed block with recovered senders.
    Recovered(Arc<RecoveredBlock>),
}

impl SealedOrRecoveredBlock {
    /// Creates a [`SealedOrRecoveredBlock`] from a sealed block.
    #[inline]
    pub fn sealed(block: SealedBlock) -> Self {
        Self::Sealed(Arc::new(block))
    }

    /// Creates a [`SealedOrRecoveredBlock`] from a shared sealed block.
    #[inline]
    pub const fn sealed_arc(block: Arc<SealedBlock>) -> Self {
        Self::Sealed(block)
    }

    /// Creates a [`SealedOrRecoveredBlock`] from a recovered block.
    #[inline]
    pub fn recovered(block: RecoveredBlock) -> Self {
        Self::Recovered(Arc::new(block))
    }

    /// Creates a [`SealedOrRecoveredBlock`] from a shared recovered block.
    #[inline]
    pub const fn recovered_arc(block: Arc<RecoveredBlock>) -> Self {
        Self::Recovered(block)
    }

    /// Returns the sealed block view.
    #[inline]
    pub fn sealed_block(&self) -> &SealedBlock {
        match self {
            Self::Sealed(block) => block,
            Self::Recovered(block) => block.sealed_block(),
        }
    }

    /// Returns the recovered block if this block has recovered senders.
    #[inline]
    pub fn recovered_block(&self) -> Option<&RecoveredBlock> {
        match self {
            Self::Sealed(_) => None,
            Self::Recovered(block) => Some(block),
        }
    }

    /// Consumes this block and returns the sealed block.
    pub fn into_sealed_block(self) -> SealedBlock {
        match self {
            Self::Sealed(block) => Arc::unwrap_or_clone(block),
            Self::Recovered(block) => match Arc::try_unwrap(block) {
                Ok(block) => block.into_sealed_block(),
                Err(block) => block.clone_sealed_block(),
            },
        }
    }

    /// Consumes this block and returns the recovered block, recovering sealed-only blocks if
    /// needed.
    pub fn into_recovered_block(self) -> Result<RecoveredBlock, SealedBlockRecoveryError> {
        match self {
            Self::Sealed(block) => Arc::unwrap_or_clone(block).try_recover(),
            Self::Recovered(block) => Ok(Arc::unwrap_or_clone(block)),
        }
    }
}

impl PartialEq for SealedOrRecoveredBlock {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.sealed_block().eq(other.sealed_block())
    }
}

impl Eq for SealedOrRecoveredBlock {}

impl From<SealedBlock> for SealedOrRecoveredBlock {
    #[inline]
    fn from(block: SealedBlock) -> Self {
        Self::sealed(block)
    }
}

impl From<Arc<SealedBlock>> for SealedOrRecoveredBlock {
    #[inline]
    fn from(block: Arc<SealedBlock>) -> Self {
        Self::sealed_arc(block)
    }
}

impl From<RecoveredBlock> for SealedOrRecoveredBlock {
    #[inline]
    fn from(block: RecoveredBlock) -> Self {
        Self::recovered(block)
    }
}

impl From<Arc<RecoveredBlock>> for SealedOrRecoveredBlock {
    #[inline]
    fn from(block: Arc<RecoveredBlock>) -> Self {
        Self::recovered_arc(block)
    }
}

impl Deref for SealedOrRecoveredBlock {
    type Target = SealedBlock;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.sealed_block()
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for SealedOrRecoveredBlock
where
    SealedBlock: serde::Serialize,
{
    #[inline]
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.sealed_block().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for SealedOrRecoveredBlock
where
    SealedBlock: serde::Deserialize<'de>,
{
    #[inline]
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        SealedBlock::deserialize(deserializer).map(Self::sealed)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use base_common_consensus::BaseBlock as TestBlock;

    use super::*;

    fn sealed_block() -> SealedBlock {
        SealedBlock::seal_slow(TestBlock::default())
    }

    #[test]
    fn sealed_variant_returns_sealed_block() {
        let sealed = sealed_block();
        let hash = sealed.hash();
        let block = SealedOrRecoveredBlock::sealed(sealed);

        assert_eq!(block.hash(), hash);
        assert!(block.recovered_block().is_none());
    }

    #[test]
    fn recovered_variant_returns_recovered_block() {
        let recovered = sealed_block().with_senders(Vec::new());
        let hash = recovered.hash();
        let block = SealedOrRecoveredBlock::recovered(recovered);

        assert_eq!(block.hash(), hash);
        assert!(block.recovered_block().is_some());
        assert_eq!(block.into_sealed_block().hash(), hash);
    }

    #[test]
    fn sealed_and_recovered_variants_compare_by_sealed_block() {
        let sealed = sealed_block();
        let recovered = sealed.clone().with_senders(Vec::new());

        assert_eq!(
            SealedOrRecoveredBlock::sealed(sealed),
            SealedOrRecoveredBlock::recovered(recovered)
        );
    }
}
