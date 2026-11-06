use std::sync::Arc;

use alloy_consensus::{Header, Receipt, Sealed, TxEnvelope};
use alloy_eips::eip4844::Blob;
use alloy_primitives::B256;
use alloy_transport::TransportErrorKind;
use async_trait::async_trait;
use base_consensus_derive::{BlobProvider, BlobProviderError, ChainProvider, PipelineErrorKind};
use base_consensus_l1::L1Provider;
use base_consensus_providers::{AlloyChainProvider, OnlineBeaconClient, OnlineBlobProvider};
use base_protocol::BlockInfo;
use tokio::sync::watch;

use crate::{
    DelayedL1OriginSelectorProvider, L1OriginSelectorError, L1OriginSelectorProvider,
    PreparedL1Origin,
};

/// L1 chain data from an L1 RPC or from an [`L1Provider`].
#[derive(Debug, Clone)]
pub enum L1ChainProvider {
    /// An L1 execution RPC.
    Rpc(AlloyChainProvider),
    /// An [`L1Provider`], read in process.
    Node(L1Provider),
}

#[async_trait]
impl ChainProvider for L1ChainProvider {
    type Error = PipelineErrorKind;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        match self {
            Self::Rpc(rpc) => rpc.header_by_hash(hash).await.map_err(Into::into),
            Self::Node(l1) => l1.header_by_hash(hash).await.map_err(Into::into),
        }
    }

    async fn block_info_by_number(&mut self, number: u64) -> Result<BlockInfo, Self::Error> {
        match self {
            Self::Rpc(rpc) => rpc.block_info_by_number(number).await.map_err(Into::into),
            Self::Node(l1) => l1.block_info_by_number(number).await.map_err(Into::into),
        }
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        match self {
            Self::Rpc(rpc) => rpc.receipts_by_hash(hash).await.map_err(Into::into),
            Self::Node(l1) => l1.receipts_by_hash(hash).await.map_err(Into::into),
        }
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        match self {
            Self::Rpc(rpc) => {
                rpc.block_info_and_transactions_by_hash(hash).await.map_err(Into::into)
            }
            Self::Node(l1) => {
                l1.block_info_and_transactions_by_hash(hash).await.map_err(Into::into)
            }
        }
    }
}

/// Blobs from an L1 beacon API or from an [`L1Provider`].
#[derive(Debug, Clone)]
pub enum L1BlobProvider {
    /// An L1 beacon API.
    Rpc(OnlineBlobProvider<OnlineBeaconClient>),
    /// An [`L1Provider`], read in process.
    Node(L1Provider),
}

#[async_trait]
impl BlobProvider for L1BlobProvider {
    type Error = BlobProviderError;

    async fn get_and_validate_blobs(
        &mut self,
        block_ref: &BlockInfo,
        blob_hashes: &[B256],
    ) -> Result<Vec<Box<Blob>>, Self::Error> {
        match self {
            Self::Rpc(rpc) => rpc.get_and_validate_blobs(block_ref, blob_hashes).await,
            Self::Node(l1) => l1.get_and_validate_blobs(block_ref, blob_hashes).await,
        }
    }
}

/// Sequencer L1 origins from an L1 RPC or from an [`L1NodeOriginProvider`].
#[derive(Debug)]
pub enum L1OriginProvider {
    /// An L1 execution RPC.
    Rpc(DelayedL1OriginSelectorProvider),
    /// An [`L1NodeOriginProvider`], read in process.
    Node(L1NodeOriginProvider),
}

#[async_trait]
impl L1OriginSelectorProvider for L1OriginProvider {
    fn chain_view(&self) -> Option<B256> {
        match self {
            Self::Rpc(rpc) => rpc.chain_view(),
            Self::Node(l1) => l1.chain_view(),
        }
    }

    async fn prepared_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
        match self {
            Self::Rpc(rpc) => rpc.prepared_by_hash(hash).await,
            Self::Node(l1) => l1.prepared_by_hash(hash).await,
        }
    }

    async fn prepared_by_number(
        &self,
        number: u64,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
        match self {
            Self::Rpc(rpc) => rpc.prepared_by_number(number).await,
            Self::Node(l1) => l1.prepared_by_number(number).await,
        }
    }
}

/// Prepares sequencer L1 origins from an [`L1Provider`], delayed like
/// [`DelayedL1OriginSelectorProvider`] by a confirmation depth behind the observed L1 head.
#[derive(Debug)]
pub struct L1NodeOriginProvider {
    /// The L1 provider.
    pub l1: L1Provider,
    /// The L1 head watch channel.
    pub l1_head: watch::Receiver<Option<BlockInfo>>,
    /// Blocks kept between the L1 head and a successor origin.
    pub confirmation_depth: u64,
}

impl L1NodeOriginProvider {
    /// Returns `header` with its receipts, or without them if peers cannot serve them yet.
    pub async fn prepare(&self, header: Sealed<Header>) -> PreparedL1Origin {
        let receipts = match self.l1.receipts(&header).await {
            Ok(receipts) => Some(Arc::new(receipts)),
            Err(error) => {
                warn!(target: "l1_origin_selector", error = %error, l1_origin = %header.hash(), "L1 receipts unavailable while preparing origin");
                None
            }
        };
        let (header, hash) = header.into_parts();
        PreparedL1Origin { hash, header, receipts }
    }
}

#[async_trait]
impl L1OriginSelectorProvider for L1NodeOriginProvider {
    fn chain_view(&self) -> Option<B256> {
        self.l1_head.borrow().as_ref().map(|head| head.hash)
    }

    async fn prepared_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
        let header =
            self.l1.node.canonical_header(hash).await.map_err(TransportErrorKind::custom)?;
        match header {
            Some(header) => Ok(Some(self.prepare(header).await)),
            None => Ok(None),
        }
    }

    async fn prepared_by_number(
        &self,
        number: u64,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
        let Some(l1_head) = *self.l1_head.borrow() else {
            return Ok(None);
        };
        if number != 0
            && self.confirmation_depth != 0
            && number.saturating_add(self.confirmation_depth) > l1_head.number
        {
            return Ok(None);
        }
        let header =
            self.l1.node.header_by_number(number).await.map_err(TransportErrorKind::custom)?;
        match header {
            Some(header) => Ok(Some(self.prepare(header).await)),
            None => Ok(None),
        }
    }
}
