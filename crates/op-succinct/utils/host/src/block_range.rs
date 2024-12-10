use std::{
    cmp::{max, min},
    collections::HashSet,
    time::Duration,
};

use crate::fetcher::{OPSuccinctDataFetcher, RPCMode};
use alloy_eips::BlockId;
use anyhow::{bail, Result};
use futures::StreamExt;
use op_alloy_rpc_types::{OutputResponse, SafeHeadResponse};
use serde::{Deserialize, Serialize};

/// Get the start and end block numbers for a range, with validation.
pub async fn get_validated_block_range(
    data_fetcher: &OPSuccinctDataFetcher,
    start: Option<u64>,
    end: Option<u64>,
    default_range: u64,
) -> Result<(u64, u64)> {
    // If safeDB is activated, get the L2 safe head. If not, use the finalized block.
    let safe_db_activated = data_fetcher.is_safe_db_activated().await?;
    let end_number = if safe_db_activated {
        let header = data_fetcher.get_l1_header(BlockId::latest()).await?;
        let safe_head_response: SafeHeadResponse = data_fetcher
            .fetch_rpc_data_with_mode(
                RPCMode::L2Node,
                "optimism_safeHeadAtL1Block",
                vec![format!("0x{:x}", header.number).into()],
            )
            .await?;
        safe_head_response.safe_head.number
    } else {
        let header = data_fetcher.get_l2_header(BlockId::finalized()).await?;
        header.number
    };

    // If end block not provided, use latest finalized block
    let l2_end_block = match end {
        Some(end) => {
            if end > end_number {
                bail!(
                    "The end block ({}) is greater than the latest finalized block ({})",
                    end,
                    end_number
                );
            }
            end
        }
        None => end_number,
    };

    // If start block not provided, use end block - default_range
    let l2_start_block = match start {
        Some(start) => start,
        None => max(1, l2_end_block.saturating_sub(default_range)),
    };

    if l2_start_block >= l2_end_block {
        bail!(
            "Start block ({}) must be less than end block ({})",
            l2_start_block,
            l2_end_block
        );
    }

    Ok((l2_start_block, l2_end_block))
}

/// Get a fixed recent (less than the provided interval) block range.
pub async fn get_rolling_block_range(
    data_fetcher: &OPSuccinctDataFetcher,
    interval: Duration,
    range: u64,
) -> Result<(u64, u64)> {
    let header = data_fetcher.get_l2_header(BlockId::finalized()).await?;
    let start_timestamp = header.timestamp - (header.timestamp % interval.as_secs());
    let (_, l2_start_block) = data_fetcher
        .find_l2_block_by_timestamp(start_timestamp)
        .await?;

    Ok((l2_start_block, l2_start_block + range))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanBatchRange {
    pub start: u64,
    pub end: u64,
}

/// Split a range of blocks into a list of span batch ranges.
///
/// This is a simple implementation used when the safeDB is not activated on the L2 Node.
pub fn split_range_basic(start: u64, end: u64, max_range_size: u64) -> Vec<SpanBatchRange> {
    let mut ranges = Vec::new();
    let mut current_start = start;

    while current_start < end {
        let current_end = min(current_start + max_range_size, end);
        ranges.push(SpanBatchRange {
            start: current_start,
            end: current_end,
        });
        current_start = current_end;
    }

    ranges
}

/// Split a range of blocks into a list of span batch ranges based on L2 safeHeads.
///
/// 1. Get the L1 block range [L1 origin of l2_start, L1Head] where L1Head is the block from which l2_end can be derived
/// 2. Loop over L1 blocks to get safeHead increases (batch posts) which form a step function
/// 3. Split ranges based on safeHead increases and max batch size
///
/// Example: If safeHeads are [27,49,90] and max_size=30, ranges will be [(0,27), (27,49), (49,69), (69,90)]
pub async fn split_range_based_on_safe_heads(
    l2_start: u64,
    l2_end: u64,
    max_range_size: u64,
) -> Result<Vec<SpanBatchRange>> {
    let data_fetcher = OPSuccinctDataFetcher::default();

    // Get the L1 origin of l2_start
    let l2_start_hex = format!("0x{:x}", l2_start);
    let start_output: OutputResponse = data_fetcher
        .fetch_rpc_data_with_mode(
            RPCMode::L2Node,
            "optimism_outputAtBlock",
            vec![l2_start_hex.into()],
        )
        .await?;
    let l1_start = start_output.block_ref.l1_origin.number;

    // Get the L1Head from which l2_end can be derived
    let (_, l1_head_number) = data_fetcher.get_l1_head_with_safe_head(l2_end).await?;

    // Get all the unique safeHeads between l1_start and l1_head
    let mut ranges = Vec::new();
    let mut current_l2_start = l2_start;
    let safe_heads = futures::stream::iter(l1_start..=l1_head_number)
        .map(|block| async move {
            let l1_block_hex = format!("0x{:x}", block);
            let data_fetcher = OPSuccinctDataFetcher::default();
            let result: SafeHeadResponse = data_fetcher
                .fetch_rpc_data_with_mode(
                    RPCMode::L2Node,
                    "optimism_safeHeadAtL1Block",
                    vec![l1_block_hex.into()],
                )
                .await
                .expect("Failed to fetch safe head");
            result.safe_head.number
        })
        .buffered(15)
        .collect::<HashSet<_>>()
        .await;

    // Collect and sort the safe heads.
    let mut safe_heads: Vec<_> = safe_heads.into_iter().collect();
    safe_heads.sort();

    // Loop over all of the safe heads and create ranges.
    for safe_head in safe_heads {
        if safe_head > current_l2_start && current_l2_start < l2_end {
            let mut range_start = current_l2_start;
            while range_start + max_range_size < min(l2_end, safe_head) {
                ranges.push(SpanBatchRange {
                    start: range_start,
                    end: range_start + max_range_size,
                });
                range_start += max_range_size;
            }
            ranges.push(SpanBatchRange {
                start: range_start,
                end: min(l2_end, safe_head),
            });
            current_l2_start = safe_head;
        }
    }

    Ok(ranges)
}
