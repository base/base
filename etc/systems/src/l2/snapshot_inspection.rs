//! Read-only inspection of the labeled heads of a snapshot source execution node.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig};
use base_protocol::{BlockInfo, L2BlockInfo};
use eyre::{Result, ensure};
use serde::Serialize;
use url::Url;

use super::SnapshotBoundary;

/// Rollup metadata decoded from one labeled snapshot block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotLabeledBlock {
    /// L2 block identity, L1 origin, and sequence number from the block's L1-info deposit.
    pub block_info: L2BlockInfo,
    /// Effective system configuration recovered from the block.
    pub system_config: SystemConfig,
}

impl From<SnapshotBoundary> for SnapshotLabeledBlock {
    fn from(boundary: SnapshotBoundary) -> Self {
        Self { block_info: boundary.l2_block_info, system_config: boundary.system_config }
    }
}

/// Finalized, safe, and latest heads of a snapshot source node, in non-decreasing height order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotInspection {
    /// L2 chain ID reported by the node, equal to the expected chain ID.
    pub chain_id: u64,
    /// Rollup genesis copied from the trusted configuration, not read from the node.
    ///
    /// Snapshot nodes may prune the genesis body. A matching chain ID and decodable L1-info
    /// deposits on each head do not prove that those heads descend from this genesis.
    pub genesis: ChainGenesis,
    /// Rollup configuration the heads were decoded with, unchanged.
    pub rollup_config: RollupConfig,
    /// Unsafe head.
    pub latest: SnapshotLabeledBlock,
    /// Safe head.
    pub safe: SnapshotLabeledBlock,
    /// Finalized head.
    pub finalized: SnapshotLabeledBlock,
    /// Canonical L1 block that completes derivation of the unsafe tail, when fork discovery ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork: Option<BlockInfo>,
}

impl SnapshotInspection {
    /// Reads the finalized, safe, and latest heads over `rpc_url`.
    ///
    /// Fails when the chain ID does not match, when any label is missing or lacks a real L1-info
    /// deposit, such as a pruned body, or when heights violate `finalized <= safe <= latest`.
    pub async fn read(
        rpc_url: Url,
        rollup_config: Arc<RollupConfig>,
        expected_chain_id: u64,
    ) -> Result<Self> {
        // Labels only advance, so reading the most stable label first keeps a progressing node's
        // labels ordered across the separate requests.
        let read = |tag| {
            SnapshotBoundary::read(
                rpc_url.clone(),
                Arc::clone(&rollup_config),
                expected_chain_id,
                tag,
                None,
            )
        };
        let finalized = read(BlockNumberOrTag::Finalized).await?;
        let safe = read(BlockNumberOrTag::Safe).await?;
        let latest = read(BlockNumberOrTag::Latest).await?;
        ensure!(
            finalized.head.number <= safe.head.number && safe.head.number <= latest.head.number,
            "snapshot labels are out of order: finalized {}, safe {}, latest {}",
            finalized.head.number,
            safe.head.number,
            latest.head.number
        );

        Ok(Self {
            chain_id: expected_chain_id,
            genesis: rollup_config.genesis,
            rollup_config: (*rollup_config).clone(),
            latest: latest.into(),
            safe: safe.into(),
            finalized: finalized.into(),
            fork: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Block, BlockBody, Header};
    use alloy_primitives::B256;
    use base_common_consensus::BaseTxEnvelope;

    use super::SnapshotInspection;
    use crate::test_utils::SnapshotRpcFixture;

    const CHAIN_ID: u64 = SnapshotRpcFixture::CHAIN_ID;

    async fn inspect(fixture: SnapshotRpcFixture) -> eyre::Result<SnapshotInspection> {
        let rollup_config = Arc::new(fixture.rollup_config());
        let (url, handle) = fixture.serve(CHAIN_ID).await;
        let result = SnapshotInspection::read(url, rollup_config, CHAIN_ID).await;
        handle.stop().unwrap();
        result
    }

    #[tokio::test]
    async fn reads_distinct_labels_as_json_without_genesis_body() {
        let mut fixture = SnapshotRpcFixture::default();
        let rollup_config = fixture.rollup_config();
        fixture.blocks.remove("0x0");

        let json = serde_json::to_value(inspect(fixture).await.unwrap()).unwrap();
        assert_eq!(json["chain_id"], CHAIN_ID);
        assert_eq!(json["genesis"], serde_json::to_value(rollup_config.genesis).unwrap());
        assert_eq!(json["rollup_config"], serde_json::to_value(&rollup_config).unwrap());
        for (label, number, l1_number, seq_num) in
            [("finalized", 1, 10, 0), ("safe", 2, 10, 1), ("latest", 3, 11, 0)]
        {
            let block_info = &json[label]["block_info"];
            assert_eq!(block_info["number"], number, "{label}");
            assert_eq!(block_info["l1origin"]["number"], l1_number, "{label}");
            assert_eq!(
                block_info["l1origin"]["hash"],
                serde_json::to_value(B256::repeat_byte(l1_number as u8)).unwrap(),
                "{label}"
            );
            assert_eq!(block_info["sequenceNumber"], seq_num, "{label}");
            assert_eq!(
                json[label]["system_config"]["batcherAddr"],
                serde_json::to_value(SnapshotRpcFixture::BATCHER).unwrap(),
                "{label}"
            );
        }
        assert!(json.get("fork").is_none(), "fork is reported only by fork discovery");
    }

    #[tokio::test]
    async fn accepts_equal_heads() {
        let mut fixture = SnapshotRpcFixture::default();
        let latest = fixture.blocks["latest"].clone();
        fixture.blocks.insert("finalized".to_string(), latest.clone());
        fixture.blocks.insert("safe".to_string(), latest);

        let inspection = inspect(fixture).await.unwrap();
        assert_eq!(inspection.finalized, inspection.latest);
        assert_eq!(inspection.safe, inspection.latest);
    }

    #[tokio::test]
    async fn rejects_reversed_labels() {
        // Each case breaks exactly one inequality of `finalized <= safe <= latest`.
        for (label, source, heights) in [
            ("finalized", "latest", "finalized 3, safe 2, latest 3"),
            ("latest", "finalized", "finalized 1, safe 2, latest 1"),
        ] {
            let mut fixture = SnapshotRpcFixture::default();
            let block = fixture.blocks[source].clone();
            fixture.blocks.insert(label.to_string(), block);

            let error = inspect(fixture).await.unwrap_err();
            assert!(error.to_string().ends_with(heights), "{label}: {error:?}");
        }
    }

    #[tokio::test]
    async fn rejects_missing_or_pruned_label() {
        for label in ["finalized", "safe", "latest"] {
            let mut fixture = SnapshotRpcFixture::default();
            fixture.blocks.remove(label);

            let error = inspect(fixture).await.unwrap_err();
            assert!(error.to_string().contains(&format!("has no {label} block")), "{error:?}");
        }

        let mut fixture = SnapshotRpcFixture::default();
        let pruned = Block::<BaseTxEnvelope> {
            header: Header { number: 3, ..Default::default() },
            body: BlockBody::default(),
        };
        fixture.blocks.insert("latest".to_string(), SnapshotRpcFixture::rpc_block(pruned));
        let error = inspect(fixture).await.unwrap_err();
        assert!(error.to_string().contains("failed to derive L2 block info"), "{error:?}");
    }
}
