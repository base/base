use std::collections::VecDeque;

use alloy_eips::eip4844::Blob;
use alloy_primitives::{Address, B256, Bytes};
use async_trait::async_trait;
use base_consensus_derive::{
    BlobProvider, BlobProviderError, DataAvailabilityProvider, PipelineError, PipelineResult,
};
use base_protocol::BlockInfo;

use crate::SharedL1Chain;

/// In-memory blob provider backed by [`SharedL1Chain`] blob sidecars.
///
/// Implements [`BlobProvider`] for action tests that use EIP-4844 blob
/// submission. Blobs are stored in [`L1Block::blob_sidecars`](crate::L1Block)
/// when enqueued via [`L1Miner::enqueue_blob`](crate::L1Miner::enqueue_blob)
/// and looked up here by versioned hash.
#[derive(Debug, Clone)]
pub struct ActionBlobProvider {
    chain: SharedL1Chain,
}

impl ActionBlobProvider {
    /// Create a new provider backed by the given shared chain.
    pub const fn new(chain: SharedL1Chain) -> Self {
        Self { chain }
    }
}

#[async_trait]
impl BlobProvider for ActionBlobProvider {
    type Error = BlobProviderError;

    async fn get_and_validate_blobs(
        &mut self,
        block_ref: &BlockInfo,
        blob_hashes: &[B256],
    ) -> Result<Vec<Box<Blob>>, Self::Error> {
        let block = self
            .chain
            .get_block(block_ref.number)
            .filter(|b| b.hash() == block_ref.hash)
            .ok_or_else(|| {
            BlobProviderError::Backend(
                format!("block {} not found in chain", block_ref.number).into(),
            )
        })?;

        let mut blobs = Vec::new();
        for hash in blob_hashes {
            let blob = block
                .blob_sidecars
                .iter()
                .find(|(h, _)| h == hash)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| {
                    BlobProviderError::Backend(
                        format!("blob {hash} not found in block {}", block_ref.number).into(),
                    )
                })?;
            blobs.push(blob);
        }
        Ok(blobs)
    }
}

/// In-memory data availability source that reads blob sidecars from
/// [`SharedL1Chain`].
///
/// Implements [`DataAvailabilityProvider`] for action tests that submit batch
/// data as EIP-4844 blobs rather than calldata. The blob bytes are delivered
/// raw (the full 128-KiB blob field).
///
/// This source also reads calldata `batcher_txs` so a single source can serve
/// tests that mix calldata and blob submissions.
#[derive(Debug, Clone)]
pub struct ActionBlobDataSource {
    chain: SharedL1Chain,
    /// The expected batch inbox address.
    inbox_address: Address,
    /// Queued payload bytes for the current block (both calldata and blob data).
    pending: VecDeque<Bytes>,
    /// Whether the current block's data has been loaded.
    open: bool,
}

impl ActionBlobDataSource {
    /// Create a new blob data source backed by the given shared chain.
    pub const fn new(chain: SharedL1Chain, inbox_address: Address) -> Self {
        Self { chain, inbox_address, pending: VecDeque::new(), open: false }
    }

    fn load_block(&mut self, block_ref: &BlockInfo, batcher_address: Address) {
        let Some(block) = self.chain.get_block(block_ref.number) else { return };
        // Guard against stale block_refs after a reorg.
        if block.hash() != block_ref.hash {
            self.open = true;
            return;
        }
        // Calldata path.
        for tx in &block.batcher_txs {
            if tx.from == batcher_address && tx.to == self.inbox_address {
                self.pending.push_back(tx.input.clone());
            }
        }
        // Blob path: emit raw blob bytes for each sidecar.
        for (_, blob) in &block.blob_sidecars {
            self.pending.push_back(Bytes::copy_from_slice(blob.as_slice()));
        }
        self.open = true;
    }
}

#[async_trait]
impl DataAvailabilityProvider for ActionBlobDataSource {
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
