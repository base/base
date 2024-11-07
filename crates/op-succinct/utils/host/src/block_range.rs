use crate::fetcher::OPSuccinctDataFetcher;
use alloy_eips::BlockId;
use anyhow::{bail, Result};

/// Get the start and end block numbers for a range, with validation.
pub async fn get_validated_block_range(
    data_fetcher: &OPSuccinctDataFetcher,
    start: Option<u64>,
    end: Option<u64>,
    default_range: u64,
) -> Result<(u64, u64)> {
    let header = data_fetcher.get_l2_header(BlockId::finalized()).await?;

    // If end block not provided, use latest finalized block
    let l2_end_block = match end {
        Some(end) => {
            if end > header.number {
                bail!(
                    "The end block ({}) is greater than the latest finalized block ({})",
                    end,
                    header.number
                );
            }
            end
        }
        None => header.number,
    };

    // If start block not provided, use end block - default_range
    let l2_start_block = match start {
        Some(start) => start,
        None => l2_end_block.saturating_sub(default_range),
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
