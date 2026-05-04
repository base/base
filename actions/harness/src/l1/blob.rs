use alloy_eips::eip4844::Blob;
use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{BlobProvider, BlobProviderError};
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
            BlobProviderError::Backend(format!("block {} not found in chain", block_ref.number))
        })?;

        let mut blobs = Vec::new();
        for hash in blob_hashes {
            let blob = block
                .blob_sidecars
                .iter()
                .find(|(h, _)| h == hash)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| {
                    BlobProviderError::Backend(format!(
                        "blob {hash} not found in block {}",
                        block_ref.number
                    ))
                })?;
            blobs.push(blob);
        }
        Ok(blobs)
    }
}
