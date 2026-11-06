use std::sync::Arc;

use alloy_consensus::{Header, Receipt, Sealed, TxEnvelope};
use alloy_eips::{BlockId, eip4844::Blob};
use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{
    BlobDecodingError, BlobProvider, BlobProviderError, ChainProvider, PipelineError,
    PipelineErrorKind, ResetError,
};
use base_protocol::BlockInfo;

use crate::{BeaconPeers, L1Node, L1PeersError};

/// Errors from [`L1Provider`].
#[derive(Debug, thiserror::Error)]
pub enum L1ProviderError {
    /// Peers did not serve the block data.
    #[error(transparent)]
    Peers(#[from] L1PeersError),
    /// The block is not on the verified canonical chain.
    #[error("L1 block not found: {0}")]
    BlockNotFound(BlockId),
}

impl From<L1ProviderError> for PipelineErrorKind {
    fn from(error: L1ProviderError) -> Self {
        match error {
            L1ProviderError::Peers(error) => PipelineError::Provider(error.to_string()).temp(),
            // A missing hash was reorged out, so retrying cannot succeed; a missing number is
            // not produced yet.
            L1ProviderError::BlockNotFound(id @ BlockId::Hash(_)) => {
                ResetError::BlockNotFound(id).reset()
            }
            L1ProviderError::BlockNotFound(id) => {
                PipelineError::Provider(format!("L1 block not found: {id}")).temp()
            }
        }
    }
}

/// Serves the consensus node's L1 chain and blob reads straight from an [`L1Node`], with no
/// RPC encoding in between.
#[derive(Debug, Clone)]
pub struct L1Provider {
    /// Verified block data.
    pub node: Arc<L1Node>,
    /// Peers blobs are rebuilt from.
    pub beacon: BeaconPeers,
}

impl L1Provider {
    /// Returns the canonical header with `hash`.
    pub async fn canonical(&self, hash: B256) -> Result<Sealed<Header>, L1ProviderError> {
        self.node
            .canonical_header(hash)
            .await?
            .ok_or_else(|| L1ProviderError::BlockNotFound(hash.into()))
    }

    /// Returns the [`BlockInfo`] of `header`.
    pub fn block_info(header: &Sealed<Header>) -> BlockInfo {
        BlockInfo::new(header.hash(), header.number, header.parent_hash, header.timestamp)
    }

    /// Returns the consensus receipts of `header`.
    pub async fn receipts(&self, header: &Sealed<Header>) -> Result<Vec<Receipt>, L1PeersError> {
        let receipts = self.node.receipts(header).await?;
        Ok(receipts
            .iter()
            .map(|receipt| Receipt {
                status: receipt.success.into(),
                cumulative_gas_used: receipt.cumulative_gas_used,
                logs: receipt.logs.clone(),
            })
            .collect())
    }
}

#[async_trait]
impl ChainProvider for L1Provider {
    type Error = L1ProviderError;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        Ok(self.canonical(hash).await?.into_inner())
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        let header = self
            .node
            .header_by_number(number)
            .await?
            .ok_or_else(|| L1ProviderError::BlockNotFound(number.into()))?;
        Ok(Self::block_info(&header))
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        let header = self.canonical(hash).await?;
        Ok(self.receipts(&header).await?)
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        let header = self.canonical(hash).await?;
        let body = self.node.body(&header).await?;
        let transactions = body.transactions.iter().cloned().map(Into::into).collect();
        Ok((Self::block_info(&header), transactions))
    }
}

#[async_trait]
impl BlobProvider for L1Provider {
    type Error = BlobProviderError;

    /// Blobs are checked against `blob_hashes` through their recomputed KZG commitments as they
    /// are rebuilt from data columns.
    async fn get_and_validate_blobs(
        &mut self,
        block_ref: &BlockInfo,
        blob_hashes: &[B256],
    ) -> Result<Vec<Box<Blob>>, Self::Error> {
        let schedule = &self.beacon.network.schedule;
        let slot = block_ref
            .timestamp
            .checked_sub(schedule.genesis_time)
            .ok_or(BlobProviderError::SlotDerivation)?
            / schedule.seconds_per_slot;
        let blobs = self
            .beacon
            .blobs(slot, blob_hashes)
            .await
            .map_err(|error| BlobProviderError::Backend(error.to_string()))?;
        blobs
            .iter()
            .map(|blob| {
                Blob::try_from(blob.as_ref())
                    .map(Box::new)
                    .map_err(|_| BlobDecodingError::InvalidLength.into())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_hash_resets_and_missing_number_retries() {
        let by_hash = L1ProviderError::BlockNotFound(BlockId::from(B256::ZERO));
        assert!(matches!(PipelineErrorKind::from(by_hash), PipelineErrorKind::Reset(_)));

        let by_number = L1ProviderError::BlockNotFound(BlockId::from(7u64));
        assert!(matches!(PipelineErrorKind::from(by_number), PipelineErrorKind::Temporary(_)));
    }
}
