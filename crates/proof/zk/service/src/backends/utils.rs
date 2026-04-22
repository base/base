//! Shared utilities for proving backends.

use alloy_primitives::B256;
use alloy_provider::{Identity, Provider, ProviderBuilder};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use anyhow::Result;
use base_common_network::Base;
use tracing::debug;

/// L2 output root at a specific block.
#[derive(serde::Deserialize, Debug)]
pub(super) struct OpOutputAtBlock {
    #[serde(rename = "blockRef")]
    pub block_ref: BlockRef,
}

/// Block number and hash reference.
#[derive(serde::Deserialize, Debug)]
pub(super) struct BlockRef {
    #[serde(rename = "l1origin")]
    pub l1_origin: L1Origin,
}

/// L1 origin info for an L2 block.
#[derive(serde::Deserialize, Debug)]
pub(super) struct L1Origin {
    /// L1 block number.
    pub number: u64,
}

/// Computes safe L1 head blocks for proof ranges.
#[derive(Debug)]
pub struct L1HeadCalculator;

impl L1HeadCalculator {
    /// Calculate L1 head block number and hash.
    ///
    /// Returns `(l1_head_block_number, l1_head_hash)`.
    pub async fn calculate_l1_head(
        l1_node_url: &str,
        base_consensus_url: &str,
        l2_block_number: u64,
        sequence_window: u64,
    ) -> Result<(u64, B256)> {
        debug!(
            l1_node_url = l1_node_url,
            base_consensus_url = base_consensus_url,
            l2_block_number = l2_block_number,
            sequence_window = sequence_window,
            "calculating L1 head"
        );

        let l1_provider = ProviderBuilder::new().connect_http(l1_node_url.parse()?);
        let op_provider = ProviderBuilder::<Identity, Identity, Base>::default()
            .connect_http(base_consensus_url.parse()?);

        let l1_origin = Self::get_l1_origin_num(&op_provider, l2_block_number).await?;
        debug!(l1_origin = l1_origin, l2_block_number = l2_block_number, "retrieved L1 origin");

        let desired_l1_head = l1_origin + sequence_window;

        let finalized_block = l1_provider
            .get_block(BlockId::Number(BlockNumberOrTag::Finalized))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Failed to get finalized L1 block"))?;

        let finalized_block_num = finalized_block.header.number;
        let l1_head_block_num = desired_l1_head.min(finalized_block_num);

        if l1_head_block_num < desired_l1_head {
            debug!(
                desired = desired_l1_head,
                finalized = finalized_block_num,
                used = l1_head_block_num,
                "capped L1 head to finalized block"
            );
        }

        let l1_head_hash = Self::get_block_hash(&l1_provider, l1_head_block_num).await?;

        debug!(
            l1_head_block_num = l1_head_block_num,
            l1_head_hash = %l1_head_hash,
            "calculated L1 head"
        );

        Ok((l1_head_block_num, l1_head_hash))
    }

    /// Get L1 origin block number from `optimism_outputAtBlock`.
    pub async fn get_l1_origin_num<OP>(op_provider: &OP, l2_block_number: u64) -> Result<u64>
    where
        OP: Provider<Base>,
    {
        let response: OpOutputAtBlock = op_provider
            .raw_request(
                "optimism_outputAtBlock".into(),
                (BlockNumberOrTag::Number(l2_block_number),),
            )
            .await?;

        Ok(response.block_ref.l1_origin.number)
    }

    /// Get block hash at a specific height.
    async fn get_block_hash<P>(provider: &P, block_number: u64) -> Result<B256>
    where
        P: Provider,
    {
        let block = provider
            .get_block(BlockId::Number(BlockNumberOrTag::Number(block_number)))
            .await?
            .ok_or_else(|| anyhow::anyhow!("L1 block {block_number} not found"))?;

        Ok(block.header.hash)
    }
}
