//! Validation and metadata extraction for a snapshot-backed L2 execution node.

use std::sync::Arc;

use alloy_consensus::Transaction as _;
use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use base_common_genesis::{RollupConfig, SystemConfig};
use base_common_network::Base;
use base_protocol::{L1BlockInfoTx, L2BlockInfo, to_system_config};
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

        let l2_block_info = L2BlockInfo::from_block_and_genesis(&block, &rollup_config.genesis)
            .wrap_err("failed to derive L2 block info from snapshot head")?;
        let first_transaction = block
            .body
            .transactions
            .first()
            .and_then(|transaction| transaction.as_deposit())
            .ok_or_eyre("snapshot head transaction zero is not an L1-info deposit")?;
        let l1_info = L1BlockInfoTx::decode_calldata(first_transaction.input().as_ref())
            .wrap_err("failed to decode snapshot head L1-info transaction")?;
        ensure!(
            l2_block_info.seq_num == l1_info.sequence_number(),
            "snapshot sequence number changed while extracting boundary metadata"
        );
        let system_config = to_system_config(&block, &rollup_config)
            .wrap_err("failed to recover system config from snapshot head")?;

        Ok(Self { head, l2_block_info, l1_info, system_config })
    }
}
