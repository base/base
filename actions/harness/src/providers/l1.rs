use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use alloy_consensus::{Header, Receipt};
use alloy_primitives::{Address, B256, Bytes};
use async_trait::async_trait;
use base_consensus_derive::{
    ChainProvider, DataAvailabilityProvider, PipelineError, PipelineErrorKind, PipelineResult,
};
use base_protocol::BlockInfo;

use crate::{L1Block, miner::block_info_from};

/// A shared, append-only view of the L1 chain for use by in-process providers.
///
/// Both [`ActionL1ChainProvider`] and [`ActionDataSource`] hold a clone of this
/// handle. Call [`SharedL1Chain::push`] after each `L1Miner::mine_block()` to
/// keep the providers up to date.
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

    fn with<R>(&self, f: impl FnOnce(&[L1Block]) -> R) -> R {
        let g = self.0.lock().expect("chain lock poisoned");
        f(&g)
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
/// attributes-builder stages. Receipts return empty since action tests do not
/// track L1 receipts or system config updates.
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
                .map(|b| (block_info_from(b), vec![]))
                .ok_or(L1ProviderError::HashNotFound)
        })
    }
}

/// In-memory data availability source backed by [`SharedL1Chain`].
///
/// Filters batcher transactions by `from == batcher_address && to == inbox_address`
/// and returns their calldata one item at a time per L1 block.
#[derive(Debug, Clone)]
pub struct ActionDataSource {
    chain: SharedL1Chain,
    /// The expected batch inbox address.
    inbox_address: Address,
    /// Queued calldata for the current block.
    pending: VecDeque<Bytes>,
    /// Whether the current block's txs have been loaded.
    open: bool,
}

impl ActionDataSource {
    /// Create a new data source backed by the given shared chain.
    pub const fn new(chain: SharedL1Chain, inbox_address: Address) -> Self {
        Self { chain, inbox_address, pending: VecDeque::new(), open: false }
    }

    fn load_block(&mut self, block_ref: &BlockInfo, batcher_address: Address) {
        self.chain.with(|blocks| {
            if let Some(block) = blocks.get(block_ref.number as usize) {
                // Guard against stale block_refs after a reorg: if the block at
                // this height was replaced, its hash will differ.
                if block.hash() != block_ref.hash {
                    return;
                }
                for tx in &block.batcher_txs {
                    if tx.from == batcher_address && tx.to == self.inbox_address {
                        self.pending.push_back(tx.input.clone());
                    }
                }
            }
        });
        self.open = true;
    }
}

#[async_trait]
impl DataAvailabilityProvider for ActionDataSource {
    type Item = Bytes;

    async fn next(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> PipelineResult<Self::Item> {
        if !self.open {
            self.load_block(block_ref, batcher_address);
        }
        self.pending.pop_front().ok_or_else(|| PipelineError::Eof.temp())
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.open = false;
    }
}
