use std::sync::{Arc, Mutex};

use alloy_consensus::{Header, Receipt};
use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{ChainProvider, PipelineError, PipelineErrorKind};
use base_consensus_node::{L1OriginSelectorError, L1OriginSelectorProvider};
use base_protocol::BlockInfo;

use crate::{L1Block, block_info_from};

/// A shared, append-only view of the L1 chain for use by in-process providers.
///
/// Call [`SharedL1Chain::push`] after each `L1Miner::mine_block()` to keep the
/// providers up to date.
#[derive(Debug, Clone, Default)]
pub struct SharedL1Chain(Arc<Mutex<Vec<L1Block>>>);

impl SharedL1Chain {
    /// Create a new chain pre-populated with the given blocks.
    pub fn from_blocks(blocks: Vec<L1Block>) -> Self {
        Self(Arc::new(Mutex::new(blocks)))
    }

    /// Append a newly mined block to the shared chain.
    pub fn push(&self, block: L1Block) {
        self.0.lock().expect("chain lock poisoned").push(block);
    }

    /// Truncate the chain to retain only blocks `0..=number`.
    ///
    /// Use this after an L1 reorg to remove orphaned blocks from the shared
    /// view before pushing replacement blocks mined on the new fork.
    pub fn truncate_to(&self, number: u64) {
        self.0.lock().expect("chain lock poisoned").truncate((number + 1) as usize);
    }

    /// Look up a block by number, returning a clone if it exists.
    pub fn get_block(&self, number: u64) -> Option<L1Block> {
        self.0.lock().expect("chain lock poisoned").get(number as usize).cloned()
    }

    /// Return the tip (latest) block, or `None` if the chain is empty.
    pub fn tip(&self) -> Option<L1Block> {
        self.0.lock().expect("chain lock poisoned").last().cloned()
    }

    /// Look up a block by hash, returning a clone if it exists.
    pub fn block_by_hash(&self, hash: alloy_primitives::B256) -> Option<L1Block> {
        self.0.lock().expect("chain lock poisoned").iter().find(|b| b.hash() == hash).cloned()
    }

    fn with<R>(&self, f: impl FnOnce(&[L1Block]) -> R) -> R {
        let g = self.0.lock().expect("chain lock poisoned");
        f(&g)
    }
}

#[async_trait]
impl L1OriginSelectorProvider for SharedL1Chain {
    async fn get_block_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError> {
        Ok(self.block_by_hash(hash).map(|b| block_info_from(&b)))
    }

    async fn get_block_by_number(
        &self,
        number: u64,
    ) -> Result<Option<BlockInfo>, L1OriginSelectorError> {
        Ok(self.get_block(number).map(|b| block_info_from(&b)))
    }
}

/// Error type for [`ActionL1ChainProvider`].
#[derive(Debug, thiserror::Error)]
pub enum L1ProviderError {
    /// Block not found by number.
    #[error("block not found: {0}")]
    BlockNotFound(u64),
    /// Block not found by hash.
    #[error("block hash not found")]
    HashNotFound,
}

impl From<L1ProviderError> for PipelineErrorKind {
    fn from(e: L1ProviderError) -> Self {
        PipelineError::Provider(e.to_string()).temp()
    }
}

/// In-memory L1 chain provider backed by [`SharedL1Chain`].
///
/// Implements [`ChainProvider`] for the derivation pipeline's traversal and
/// attributes-builder stages.
#[derive(Debug, Clone)]
pub struct ActionL1ChainProvider {
    chain: SharedL1Chain,
}

impl ActionL1ChainProvider {
    /// Create a new provider backed by the given shared chain.
    pub const fn new(chain: SharedL1Chain) -> Self {
        Self { chain }
    }
}

#[async_trait]
impl ChainProvider for ActionL1ChainProvider {
    type Error = L1ProviderError;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        self.chain.with(|blocks| {
            blocks
                .iter()
                .find(|b| b.hash() == hash)
                .map(|b| b.header.clone())
                .ok_or(L1ProviderError::HashNotFound)
        })
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        self.chain.with(|blocks| {
            blocks
                .get(number as usize)
                .map(block_info_from)
                .ok_or(L1ProviderError::BlockNotFound(number))
        })
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        self.chain.with(|blocks| {
            Ok(blocks
                .iter()
                .find(|b| b.hash() == hash)
                .map(|b| b.receipts.clone())
                .unwrap_or_default())
        })
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<alloy_consensus::TxEnvelope>), Self::Error> {
        self.chain.with(|blocks| {
            blocks
                .iter()
                .find(|b| b.hash() == hash)
                .map(|b| (block_info_from(b), b.transactions.clone()))
                .ok_or(L1ProviderError::HashNotFound)
        })
    }
}
