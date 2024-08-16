use alloy_consensus::Header;
use alloy_primitives::B256;
use anyhow::Result;
use client_utils::RawBootInfo;
use host_utils::fetcher::{ChainMode, SP1KonaDataFetcher};

/// Search through the boot_infos to find the L1 Header with the earliest block number.
async fn get_earliest_l1_head_in_batch(
    fetcher: &SP1KonaDataFetcher,
    boot_infos: &Vec<RawBootInfo>,
) -> Result<Header> {
    let mut earliest_block_num: u64 = u64::MAX;
    let mut earliest_l1_header: Option<Header> = None;

    for boot_info in boot_infos {
        let l1_block_header = fetcher
            .get_header_by_hash(ChainMode::L1, boot_info.l1_head)
            .await?;
        if l1_block_header.number < earliest_block_num {
            earliest_block_num = l1_block_header.number;
            earliest_l1_header = Some(l1_block_header);
        }
    }
    Ok(earliest_l1_header.unwrap())
}

/// Fetch the headers for all the blocks in the range from the earliest L1 Head in the boot_infos
/// through the checkpointed L1 Head.
pub async fn fetch_header_preimages(
    boot_infos: &Vec<RawBootInfo>,
    checkpoint_block_hash: B256,
) -> Result<Vec<Header>> {
    let fetcher = SP1KonaDataFetcher::new();

    // Get the earliest L1 Head from the boot_infos.
    let start_header = get_earliest_l1_head_in_batch(&fetcher, boot_infos).await?;

    // Fetch the full header for the latest L1 Head (which is validated on chain).
    let latest_header = fetcher
        .get_header_by_hash(ChainMode::L1, checkpoint_block_hash)
        .await?;

    fetcher
        .fetch_headers_in_range(start_header.number, latest_header.number)
        .await
}
