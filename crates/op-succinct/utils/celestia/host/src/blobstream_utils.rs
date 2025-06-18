use alloy_eips::BlockId;
use alloy_primitives::{keccak256, Log as PrimitiveLog, LogData, B256};
use alloy_provider::Provider;
use alloy_rpc_types::Filter;
use alloy_sol_types::SolEvent;
use anyhow::{anyhow, Result};
use hana_blobstream::blobstream::{blobstream_address, SP1Blobstream};
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct CelestiaL1SafeHead {
    pub l1_block_number: u64,
    pub l2_safe_head_number: u64,
}

impl CelestiaL1SafeHead {
    /// Get the L1 block hash for this safe head.
    pub async fn get_l1_hash(&self, fetcher: &OPSuccinctDataFetcher) -> Result<B256> {
        Ok(fetcher.get_l1_header(self.l1_block_number.into()).await?.hash_slow())
    }
}

/// Response structure from Celestia indexer RPC.
#[derive(Debug, Deserialize, Serialize)]
pub struct CelestiaLocationResponse {
    pub height: u64,
    pub commitment: String,
    pub l2_range: L2Range,
    pub l1_block: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct L2Range {
    pub start: u64,
    pub end: u64,
}

/// Decode and verify a DataCommitmentStored event to check if it contains the target Celestia
/// height. Returns true if the event contains the target height within its range.
fn verify_data_commitment_event(
    log: &alloy_rpc_types::Log,
    target_celestia_height: u64,
) -> Result<bool> {
    // Convert alloy_rpc_types::Log to alloy_primitives::Log
    let primitive_log = PrimitiveLog {
        address: log.address(),
        data: LogData::new(log.topics().to_vec(), log.data().data.clone()).unwrap(),
    };

    // Decode the DataCommitmentStored event
    let decoded_event = SP1Blobstream::DataCommitmentStored::decode_log(&primitive_log)?;

    tracing::info!(
        "Decoded DataCommitmentStored event: proof_nonce={}, start_block={}, end_block={}",
        decoded_event.proofNonce,
        decoded_event.startBlock,
        decoded_event.endBlock
    );

    // Check if the target Celestia height is within the range of this event
    let is_within_range = target_celestia_height >= decoded_event.startBlock &&
        target_celestia_height <= decoded_event.endBlock;

    if is_within_range {
        tracing::info!(
            "Found matching DataCommitmentStored event covering Celestia height {} (range: {}-{})",
            target_celestia_height,
            decoded_event.startBlock,
            decoded_event.endBlock
        );
    }

    Ok(is_within_range)
}

// Constant for block scanning configuration
const DEFAULT_FILTER_BLOCK_RANGE: u64 = 5000;

/// Find the minimum L1 block that contains a Blobstream proof for the given Celestia height.
/// Scans forward from the start block to find the first block with the proof.
async fn find_minimum_blobstream_block(
    celestia_height: u64,
    start_block: u64,
    fetcher: &OPSuccinctDataFetcher,
) -> Result<u64> {
    let filter_block_range = DEFAULT_FILTER_BLOCK_RANGE;

    // Get the Blobstream contract address for this chain
    let chain_id = fetcher.rollup_config.as_ref().unwrap().l1_chain_id;
    let blobstream_addr = blobstream_address(chain_id)
        .ok_or_else(|| anyhow!("No Blobstream address found for chain ID {}", chain_id))?;

    // Calculate event signature for DataCommitmentStored
    let event_signature = "DataCommitmentStored(uint256,uint64,uint64,bytes32)";
    let event_selector = keccak256(event_signature.as_bytes());

    // Start scanning from the indexer-provided block
    let mut current_start = start_block;
    let latest_block = fetcher.l1_provider.get_block_number().await?;

    tracing::info!(
        "Scanning for Blobstream proof for Celestia height {} starting from L1 block {}",
        celestia_height,
        start_block
    );

    loop {
        let current_end = std::cmp::min(current_start + filter_block_range - 1, latest_block);

        // Create filter for DataCommitmentStored events
        let filter = Filter::new()
            .address(blobstream_addr)
            .event_signature(event_selector)
            .from_block(current_start)
            .to_block(current_end);

        // Get logs from L1 provider
        let logs = fetcher.l1_provider.get_logs(&filter).await?;

        // Check each log to find the one containing our Celestia height
        for log in logs {
            // Verify that the log is from the Blobstream contract
            if log.address() == blobstream_addr {
                let block_number =
                    log.block_number.ok_or_else(|| anyhow!("Log missing block number"))?;

                // Decode and verify the event contains our target Celestia height
                match verify_data_commitment_event(&log, celestia_height) {
                    Ok(true) => {
                        tracing::info!(
                            "Found verified Blobstream event at L1 block {} containing Celestia height {}",
                            block_number,
                            celestia_height
                        );
                        return Ok(block_number);
                    }
                    Ok(false) => {
                        tracing::info!(
                            "DataCommitmentStored event at L1 block {} does not contain Celestia height {}, continuing search",
                            block_number,
                            celestia_height
                        );
                        // Continue searching for the correct event
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to decode DataCommitmentStored event at L1 block {}: {}, skipping",
                            block_number,
                            e
                        );
                        // Continue searching, maybe the next event is valid
                    }
                }
            }
        }

        // Move to next batch
        current_start = current_end + 1;
        if current_start > latest_block {
            return Err(anyhow!(
                "Reached latest block {} without finding Blobstream proof for Celestia height {}",
                latest_block,
                celestia_height
            ));
        }
    }
}

/// Query the Celestia indexer for the location of an L2 block.
async fn query_celestia_indexer(l2_block: u64) -> Result<Option<CelestiaLocationResponse>> {
    // Get the indexer RPC endpoint from environment variable.
    let indexer_rpc = std::env::var("CELESTIA_INDEXER_RPC").unwrap_or_else(|_| {
        // Default to localhost if not set.
        "http://localhost:57220".to_string()
    });

    // Create the RPC request.
    let client = reqwest::Client::new();
    let request_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "admin_getCelestiaLocation",
        "params": [l2_block],
        "id": 1
    });

    let response = client
        .post(&indexer_rpc)
        .json(&request_body)
        .send()
        .await
        .map_err(|e| anyhow!("Failed to query Celestia indexer: {}", e))?;

    let json_response: serde_json::Value =
        response.json().await.map_err(|e| anyhow!("Failed to parse indexer response: {}", e))?;

    // Check for errors in the RPC response.
    if let Some(error) = json_response.get("error") {
        // If the block is not found, return None instead of an error.
        if error
            .get("message")
            .and_then(|m| m.as_str())
            .is_some_and(|msg| msg.contains("not found"))
        {
            return Ok(None);
        }
        return Err(anyhow!("Indexer RPC error: {:?}", error));
    }

    // Parse the result.
    let result =
        json_response.get("result").ok_or_else(|| anyhow!("No result in indexer response"))?;

    serde_json::from_value(result.clone())
        .map(Some)
        .map_err(|e| anyhow!("Failed to parse Celestia location response: {}", e))
}

/// Find the earliest safe L1 block with Celestia batches committed via Blobstream.
/// Uses the Celestia indexer to efficiently locate the L1 block where the L2 block's
/// Celestia data has been committed to Ethereum through Blobstream.
pub async fn get_celestia_safe_head_info(
    fetcher: &OPSuccinctDataFetcher,
    l2_reference_block: u64,
) -> Result<Option<CelestiaL1SafeHead>> {
    // Query the Celestia indexer for this L2 block's location.
    match query_celestia_indexer(l2_reference_block).await {
        Ok(Some(location)) => {
            tracing::info!(
                "Celestia indexer returned location for L2 block {}: height {}, L1 block {}",
                l2_reference_block,
                location.height,
                location.l1_block
            );

            // Find the minimum L1 block that contains the Blobstream proof
            match find_minimum_blobstream_block(location.height, location.l1_block, fetcher).await {
                Ok(safe_l1_block) => {
                    tracing::info!(
                        "Using L1 block {} (Blobstream proof found) for L2 block {}",
                        safe_l1_block,
                        l2_reference_block
                    );
                    Ok(Some(CelestiaL1SafeHead {
                        l1_block_number: safe_l1_block,
                        l2_safe_head_number: l2_reference_block,
                    }))
                }
                Err(e) => {
                    // If we can't find a Blobstream proof, return an error
                    Err(anyhow!(
                        "Failed to find Blobstream proof for Celestia height {}: {}",
                        location.height,
                        e
                    ))
                }
            }
        }
        Ok(None) => {
            // Indexer doesn't have data for this block, return error
            Err(anyhow!("Celestia indexer has no data for L2 block {}", l2_reference_block))
        }
        Err(e) => Err(anyhow!("Celestia indexer error: {}", e)),
    }
}

/// Find the highest L2 block that can be safely proven given Celestia's Blobstream commitments.
/// Uses binary search from latest_proposed_block_number down to L2 finalized block, checking
/// each block with get_celestia_safe_head_info to see if Celestia data is available.
pub async fn get_highest_finalized_l2_block(
    fetcher: &OPSuccinctDataFetcher,
    latest_proposed_block_number: u64,
) -> Result<Option<u64>> {
    // Get the L2 finalized block as our lower bound
    let l2_finalized_header = fetcher.get_l2_header(BlockId::finalized()).await?;
    let l2_finalized_block = l2_finalized_header.number;

    tracing::info!(
        "Searching for highest provable L2 block between {} (latest proposed) and {} (finalized)",
        latest_proposed_block_number,
        l2_finalized_block,
    );

    // Binary search to find the highest L2 block with available Celestia data
    let mut low = latest_proposed_block_number;
    let mut high = l2_finalized_block;
    let mut result = None;

    while low <= high {
        let mid = low + (high - low) / 2;

        tracing::debug!("Testing L2 block {} for Celestia data availability", mid);

        // Check if this L2 block has Celestia data available via get_celestia_safe_head_info
        match get_celestia_safe_head_info(fetcher, mid).await {
            Ok(Some(_safe_head)) => {
                // This block has Celestia data available, try to find a higher one
                tracing::debug!("L2 block {} has Celestia data available", mid);
                result = Some(mid);
                low = mid + 1;
            }
            Ok(None) => {
                // No Celestia data for this block, search lower
                tracing::debug!("L2 block {} has no Celestia data available", mid);
                high = mid - 1;
            }
            Err(e) => {
                // Error occurred (e.g., indexer error), treat as unavailable and search lower
                tracing::warn!("Error checking L2 block {}: {}, treating as unavailable", mid, e);
                high = mid - 1;
            }
        }
    }

    if let Some(highest_block) = result {
        tracing::info!(
            "Found highest provable L2 block: {} (out of range {}-{})",
            highest_block,
            latest_proposed_block_number,
            l2_finalized_block
        );
    } else {
        tracing::warn!(
            "No provable L2 blocks found in range {}-{}",
            latest_proposed_block_number,
            l2_finalized_block,
        );
    }

    Ok(result)
}
