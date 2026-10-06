//! Validation and metadata extraction for a snapshot-backed L2 execution node.

use std::sync::Arc;

use alloy_consensus::Transaction as _;
use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use base_common_genesis::{RollupConfig, SystemConfig};
use base_common_network::Base;
use base_protocol::{L1BlockInfoTx, L2BlockInfo, to_system_config};
use eyre::{Result, WrapErr, ensure, eyre};
use url::Url;

use crate::DevnetSnapshotHead;

/// Metadata extracted from the block at a requested tag of a snapshot-backed execution node.
#[derive(Debug, Clone)]
pub struct SnapshotBoundary {
    /// Validated identity of the block read at the requested tag.
    pub head: DevnetSnapshotHead,
    /// L2 block metadata, including the real snapshot sequence number.
    pub l2_block_info: L2BlockInfo,
    /// L1-info transaction decoded from transaction zero.
    pub l1_info: L1BlockInfoTx,
    /// Effective system configuration at the boundary.
    pub system_config: SystemConfig,
}

impl SnapshotBoundary {
    /// Reads and validates snapshot metadata for the block at `tag` over a public RPC.
    ///
    /// Fails when the block is missing or lacks a real L1-info deposit, such as a pruned body or
    /// the genesis block, rather than synthesizing metadata.
    pub async fn read(
        rpc_url: Url,
        rollup_config: Arc<RollupConfig>,
        expected_chain_id: u64,
        tag: BlockNumberOrTag,
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
            .get_block_by_number(tag)
            .full()
            .await
            .wrap_err_with(|| format!("failed to read snapshot {tag} block"))?
            .ok_or_else(|| eyre!("snapshot execution node has no {tag} block"))?
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
                "snapshot {tag} block {head:?} does not match expected head {expected:?}"
            );
        }

        let l2_block_info = L2BlockInfo::from_block_and_genesis(&block, &rollup_config.genesis)
            .wrap_err_with(|| {
                format!("failed to derive L2 block info from snapshot {tag} block")
            })?;
        let first_transaction = block
            .body
            .transactions
            .first()
            .and_then(|transaction| transaction.as_deposit())
            .ok_or_else(|| {
                eyre!("snapshot {tag} block transaction zero is not an L1-info deposit")
            })?;
        let l1_info = L1BlockInfoTx::decode_calldata(first_transaction.input().as_ref())
            .wrap_err_with(|| {
                format!("failed to decode snapshot {tag} block L1-info transaction")
            })?;
        ensure!(
            l2_block_info.seq_num == l1_info.sequence_number(),
            "snapshot sequence number changed while extracting boundary metadata"
        );
        let system_config = to_system_config(&block, &rollup_config).wrap_err_with(|| {
            format!("failed to recover system config from snapshot {tag} block")
        })?;

        Ok(Self { head, l2_block_info, l1_info, system_config })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Block, BlockBody, Header};
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::B256;
    use base_common_consensus::BaseTxEnvelope;

    use super::SnapshotBoundary;
    use crate::{DevnetSnapshotHead, test_utils::SnapshotRpcFixture};

    const CHAIN_ID: u64 = SnapshotRpcFixture::CHAIN_ID;

    #[tokio::test]
    async fn reads_metadata_at_each_tag() {
        let mut fixture = SnapshotRpcFixture::default();
        let rollup_config = Arc::new(fixture.rollup_config());
        let safe_block = fixture.blocks["safe"].clone();
        fixture.blocks.insert("0x2".to_string(), safe_block);
        let (url, handle) = fixture.serve(CHAIN_ID).await;

        let mut heads = Vec::new();
        for (tag, number, l1_number, seq_num) in [
            (BlockNumberOrTag::Finalized, 1, 10, 0),
            (BlockNumberOrTag::Safe, 2, 10, 1),
            (BlockNumberOrTag::Latest, 3, 11, 0),
            (BlockNumberOrTag::Number(2), 2, 10, 1),
        ] {
            let boundary = SnapshotBoundary::read(
                url.clone(),
                Arc::clone(&rollup_config),
                CHAIN_ID,
                tag,
                None,
            )
            .await
            .unwrap();
            let info = boundary.l2_block_info;
            assert_eq!(boundary.head.number, number, "{tag}");
            assert_eq!(boundary.head.timestamp, number * 2, "{tag}");
            assert_eq!(info.block_info.hash, boundary.head.hash, "{tag}");
            assert_eq!(info.l1_origin.number, l1_number, "{tag}");
            assert_eq!(info.l1_origin.hash, B256::repeat_byte(l1_number as u8), "{tag}");
            assert_eq!(info.seq_num, seq_num, "{tag}");
            assert_eq!(boundary.system_config.batcher_address, SnapshotRpcFixture::BATCHER);
            heads.push(boundary.head);
        }
        handle.stop().unwrap();
        assert_eq!(heads[1], heads[3], "numeric tag reads the same block as its label");
    }

    #[tokio::test]
    async fn rejects_blocks_without_metadata() {
        let mut fixture = SnapshotRpcFixture::default();
        let rollup_config = Arc::new(fixture.rollup_config());
        let pruned = Block::<BaseTxEnvelope> {
            header: Header {
                number: 1,
                parent_hash: fixture.genesis.header.hash_slow(),
                ..Default::default()
            },
            body: BlockBody::default(),
        };
        fixture.blocks.insert("finalized".to_string(), SnapshotRpcFixture::rpc_block(pruned));
        fixture.blocks.remove("safe");
        let (url, handle) = fixture.serve(CHAIN_ID).await;

        for (tag, message) in [
            (BlockNumberOrTag::Safe, "snapshot execution node has no safe block"),
            (
                BlockNumberOrTag::Finalized,
                "failed to derive L2 block info from snapshot finalized block",
            ),
            (BlockNumberOrTag::Number(0), "snapshot 0x0 block transaction zero is not"),
        ] {
            let error = SnapshotBoundary::read(
                url.clone(),
                Arc::clone(&rollup_config),
                CHAIN_ID,
                tag,
                None,
            )
            .await
            .unwrap_err();
            assert!(format!("{error:#}").contains(message), "{tag}: {error:?}");
        }
        handle.stop().unwrap();
    }

    #[tokio::test]
    async fn rejects_chain_id_mismatch() {
        let fixture = SnapshotRpcFixture::default();
        let rollup_config = Arc::new(fixture.rollup_config());
        let (url, handle) = fixture.serve(CHAIN_ID + 1).await;

        let error =
            SnapshotBoundary::read(url, rollup_config, CHAIN_ID, BlockNumberOrTag::Latest, None)
                .await
                .unwrap_err();
        handle.stop().unwrap();
        assert!(error.to_string().contains("does not match expected chain ID"), "{error:?}");
    }

    #[tokio::test]
    async fn checks_expected_head_at_tag() {
        let fixture = SnapshotRpcFixture::default();
        let rollup_config = Arc::new(fixture.rollup_config());
        let (url, handle) = fixture.serve(CHAIN_ID).await;
        let read = |expected_head| {
            SnapshotBoundary::read(
                url.clone(),
                Arc::clone(&rollup_config),
                CHAIN_ID,
                BlockNumberOrTag::Safe,
                expected_head,
            )
        };

        let safe = read(None).await.unwrap().head;
        assert_eq!(read(Some(safe)).await.unwrap().head, safe);
        let wrong = DevnetSnapshotHead { number: safe.number + 1, ..safe };
        let error = read(Some(wrong)).await.unwrap_err();
        handle.stop().unwrap();
        assert!(error.to_string().contains("does not match expected head"), "{error:?}");
    }
}
