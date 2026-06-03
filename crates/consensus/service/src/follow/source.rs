use std::{fmt::Debug, sync::Arc};

use alloy_consensus::Block;
use alloy_eips::{BlockNumberOrTag, eip7685::EMPTY_REQUESTS_HASH};
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_consensus::BaseTxEnvelope;
use base_common_genesis::RollupConfig;
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
    rollup_config: Arc<RollupConfig>,
}

impl RemoteL2Client {
    /// Creates a new [`RemoteL2Client`] from a source L2 node URL.
    pub fn new(url: Url, rollup_config: Arc<RollupConfig>) -> Self {
        let provider = RootProvider::<Base>::new_http(url);
        Self { provider, rollup_config }
    }

    fn payload_from_consensus_block(
        &self,
        block_hash: alloy_primitives::B256,
        parent_beacon_block_root: Option<alloy_primitives::B256>,
        mut consensus_block: Block<BaseTxEnvelope>,
    ) -> BaseExecutionPayloadEnvelope {
        if self.rollup_config.is_isthmus_active(consensus_block.header.timestamp)
            && consensus_block.header.requests_hash.is_none()
        {
            tracing::trace!(
                block = %block_hash,
                "backfilling empty requests_hash on post-Isthmus source block"
            );
            consensus_block.header.requests_hash = Some(EMPTY_REQUESTS_HASH);
        }

        let (execution_payload, _sidecar) =
            BaseExecutionPayload::from_block_unchecked(block_hash, &consensus_block);

        BaseExecutionPayloadEnvelope { parent_beacon_block_root, execution_payload }
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

        Ok(self.payload_from_consensus_block(block_hash, parent_beacon_block_root, consensus_block))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockBody, Header, proofs};
    use alloy_primitives::B256;
    use base_common_genesis::HardForkConfig;

    use super::*;

    fn client_with_isthmus_at(isthmus_time: Option<u64>) -> RemoteL2Client {
        let rollup_config = RollupConfig {
            hardforks: HardForkConfig { isthmus_time, ..Default::default() },
            ..Default::default()
        };
        RemoteL2Client::new(
            "http://localhost:8545".parse().expect("valid test URL"),
            Arc::new(rollup_config),
        )
    }

    fn block_without_requests_hash(
        timestamp: u64,
        withdrawals_root: B256,
    ) -> Block<BaseTxEnvelope> {
        Block {
            header: Header {
                timestamp,
                withdrawals_root: Some(withdrawals_root),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::repeat_byte(0x11)),
                requests_hash: None,
                base_fee_per_gas: Some(1),
                ..Default::default()
            },
            body: BlockBody {
                transactions: Vec::new(),
                ommers: Vec::new(),
                withdrawals: Some(Vec::new().into()),
            },
        }
    }

    #[test]
    fn post_isthmus_source_block_missing_requests_hash_still_converts_to_v4() {
        let client = client_with_isthmus_at(Some(10));
        let withdrawals_root = B256::repeat_byte(0x42);
        assert_ne!(withdrawals_root, proofs::calculate_withdrawals_root(&[]));

        let payload = client.payload_from_consensus_block(
            B256::repeat_byte(0x24),
            Some(B256::repeat_byte(0x11)),
            block_without_requests_hash(10, withdrawals_root),
        );

        let BaseExecutionPayload::V4(payload_v4) = payload.execution_payload else {
            panic!("post-Isthmus source block should convert into V4 payload");
        };
        assert_eq!(payload_v4.withdrawals_root, withdrawals_root);
    }

    #[test]
    fn pre_isthmus_source_block_missing_requests_hash_remains_v3() {
        let client = client_with_isthmus_at(Some(10));
        let withdrawals_root = proofs::calculate_withdrawals_root(&[]);

        let payload = client.payload_from_consensus_block(
            B256::repeat_byte(0x24),
            Some(B256::repeat_byte(0x11)),
            block_without_requests_hash(9, withdrawals_root),
        );

        assert!(matches!(payload.execution_payload, BaseExecutionPayload::V3(_)));
    }
}
