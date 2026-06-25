use std::fmt::Debug;

use alloy_consensus::Block;
use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_consensus::BaseTxEnvelope;
use base_common_network::Base;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_protocol::BlockInfo;
use thiserror::Error;
use url::Url;

/// Error type for [`RemoteL2Client`] operations.
#[derive(Debug, Error)]
pub enum RemoteL2ClientError {
    /// Failed to fetch block from L2 EL.
    #[error("failed to fetch block at {tag}: {source}")]
    FetchBlock {
        /// The block tag that was requested.
        tag: String,
        /// The underlying transport error.
        source: alloy_transport::TransportError,
    },

    /// Block not found at the requested tag.
    #[error("block not found at {0}")]
    BlockNotFound(String),
}

/// Trait for fetching L2 block data from the remote node.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait RemoteClient: Debug + Send + Sync {
    /// Fetches the block number at the given tag.
    async fn get_block_number(&self, tag: BlockNumberOrTag) -> Result<u64, RemoteL2ClientError>;

    /// Fetches the block info at the given tag.
    async fn get_block_info(&self, tag: BlockNumberOrTag)
    -> Result<BlockInfo, RemoteL2ClientError>;

    /// Fetches a block by number and converts it to an [`BaseExecutionPayloadEnvelope`].
    async fn get_payload_by_number(
        &self,
        number: u64,
    ) -> Result<BaseExecutionPayloadEnvelope, RemoteL2ClientError>;
}

/// Client that polls a source L2 execution layer node for block data and
/// converts blocks into [`BaseExecutionPayloadEnvelope`] for engine insertion.
#[derive(Debug, Clone)]
pub struct RemoteL2Client {
    provider: RootProvider<Base>,
}

impl RemoteL2Client {
    /// Creates a new [`RemoteL2Client`] from a source L2 node URL.
    pub fn new(url: Url) -> Self {
        let provider = RootProvider::<Base>::new_http(url);
        Self { provider }
    }
}

#[async_trait]
impl RemoteClient for RemoteL2Client {
    async fn get_block_number(&self, tag: BlockNumberOrTag) -> Result<u64, RemoteL2ClientError> {
        if matches!(tag, BlockNumberOrTag::Latest) {
            return self.provider.get_block_number().await.map_err(|e| {
                RemoteL2ClientError::FetchBlock { tag: format!("{tag:?}"), source: e }
            });
        }

        self.get_block_info(tag).await.map(|block| block.number)
    }

    async fn get_block_info(
        &self,
        tag: BlockNumberOrTag,
    ) -> Result<BlockInfo, RemoteL2ClientError> {
        let block = self
            .provider
            .get_block_by_number(tag)
            .await
            .map_err(|e| RemoteL2ClientError::FetchBlock { tag: format!("{tag:?}"), source: e })?
            .ok_or_else(|| RemoteL2ClientError::BlockNotFound(format!("{tag:?}")))?;

        Ok(BlockInfo::from(&block))
    }

    async fn get_payload_by_number(
        &self,
        number: u64,
    ) -> Result<BaseExecutionPayloadEnvelope, RemoteL2ClientError> {
        let rpc_block = self
            .provider
            .get_block_by_number(number.into())
            .full()
            .await
            .map_err(|e| RemoteL2ClientError::FetchBlock { tag: format!("{number}"), source: e })?
            .ok_or_else(|| RemoteL2ClientError::BlockNotFound(format!("{number}")))?;

        let block_hash = rpc_block.header.hash;
        let parent_beacon_block_root = rpc_block.header.parent_beacon_block_root;

        let txs: Vec<BaseTxEnvelope> = rpc_block
            .transactions
            .into_transactions()
            .map(|tx| tx.inner.inner.into_inner())
            .collect();

        let consensus_block: Block<BaseTxEnvelope> = Block {
            header: rpc_block.header.inner,
            body: alloy_consensus::BlockBody {
                transactions: txs,
                ommers: vec![],
                withdrawals: rpc_block.withdrawals,
            },
        };

        let (execution_payload, _sidecar) =
            BaseExecutionPayload::from_block_unchecked(block_hash, &consensus_block);

        Ok(BaseExecutionPayloadEnvelope { parent_beacon_block_root, execution_payload })
    }
}
