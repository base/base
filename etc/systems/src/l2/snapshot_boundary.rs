//! Validation and metadata extraction for a snapshot-backed L2 execution node.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use base_common_genesis::{RollupConfig, SystemConfig};
use base_common_network::Base;
use base_protocol::{L1BlockInfoTx, L2BlockInfo, L2BlockMetadata};
use eyre::{OptionExt, Result, WrapErr, ensure};
use url::Url;

use crate::DevnetSnapshotHead;

/// Metadata extracted from the canonical head of a snapshot-backed execution node.
#[derive(Debug, Clone)]
pub struct SnapshotBoundary {
    /// Validated head identity.
    pub head: DevnetSnapshotHead,
    /// L2 block metadata, including the real snapshot sequence number.
    pub l2_block_info: L2BlockInfo,
    /// L1-info transaction decoded from transaction zero.
    pub l1_info: L1BlockInfoTx,
    /// Effective system configuration at the boundary.
    pub system_config: SystemConfig,
}

impl SnapshotBoundary {
    /// Reads and validates snapshot boundary metadata over the builder's public RPC.
    pub async fn read(
        rpc_url: Url,
        rollup_config: Arc<RollupConfig>,
        expected_chain_id: u64,
        expected_head: Option<DevnetSnapshotHead>,
    ) -> Result<Self> {
        let provider = RootProvider::<Base>::new_http(rpc_url);
        let chain_id =
            provider.get_chain_id().await.wrap_err("failed to read snapshot chain ID")?;
        ensure!(
            chain_id == expected_chain_id,
            "snapshot chain ID {chain_id} does not match expected chain ID {expected_chain_id}"
        );

        let block = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .full()
            .await
            .wrap_err("failed to read snapshot head")?
            .ok_or_eyre("snapshot execution node has no latest block")?
            .map_header(|header| header.into_inner())
            .into_consensus()
            .map_transactions(|transaction| transaction.inner.inner.into_inner());
        let head = DevnetSnapshotHead {
            number: block.header.number,
            hash: block.header.hash_slow(),
            timestamp: block.header.timestamp,
        };
        if let Some(expected) = expected_head {
            ensure!(
                head == expected,
                "snapshot head {head:?} does not match expected head {expected:?}"
            );
        }

        let metadata = L2BlockMetadata::from_block(&block, &rollup_config)
            .wrap_err("failed to decode snapshot head metadata")?;

        Ok(Self {
            head,
            l2_block_info: metadata.l2_block_info,
            l1_info: metadata.l1_info,
            system_config: metadata.system_config,
        })
    }
}
