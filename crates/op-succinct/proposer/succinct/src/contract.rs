use alloy_primitives::Address;
use anyhow::Result;
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use op_succinct_host_utils::OPSuccinctL2OutputOracle;

/// Get the latest proposed block number from the L2 output oracle.
pub async fn get_latest_proposed_block_number(
    address: Address,
    fetcher: &OPSuccinctDataFetcher,
) -> Result<u64> {
    let l2_output_oracle = OPSuccinctL2OutputOracle::new(address, fetcher.l1_provider.clone());
    let block_number = l2_output_oracle.latestBlockNumber().call().await?;

    // Convert the block number to a u64.
    let block_number = block_number._0.try_into().unwrap();
    Ok(block_number)
}
