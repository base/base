use alloy_consensus::Transaction;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use alloy_rpc_types::eth::Transaction as EthTransaction;
use anyhow::{anyhow, Result};
use hana_blobstream::blobstream::{blobstream_address, SP1Blobstream};
use kona_rpc::SafeHeadResponse;
use op_succinct_host_utils::fetcher::{OPSuccinctDataFetcher, RPCMode};

/// Extract the Celestia height from batcher transaction based on the version byte.
///
/// Returns:
/// - Some(height) if the transaction is a valid Celestia batcher transaction.
/// - None if the transaction is an ETH DA transaction (EIP4844 transaction or non-EIP4844
///   transaction with version byte 0x00).
/// - Err if the version byte is invalid, the commitment type is incorrect, or the da layer byte is
///   incorrect for non-EIP4844 transactions.
pub fn extract_celestia_height(tx: &EthTransaction) -> Result<Option<u64>> {
    // Skip calldata parsing for EIP4844 transactions since there is no calldata.
    if tx.inner.is_eip4844() {
        Ok(None)
    } else {
        let calldata = tx.input();

        // Check minimum calldata length for version byte.
        if calldata.is_empty() {
            return Err(anyhow!("Calldata is empty, cannot extract version byte"));
        }

        // Check version byte to determine if it is ETH DA or Alt DA.
        // https://specs.optimism.io/protocol/derivation.html#batcher-transaction-format.
        match calldata[0] {
            0x00 => Ok(None), // ETH DA transaction.
            0x01 => {
                // Check minimum length for Celestia DA transaction:
                // [0] = version byte (0x01)
                // [1] = commitment type (0x01 for altda commitment)
                // [2] = da layer byte (0x0c for Celestia)
                // [3..11] = 8-byte Celestia height (little-endian)
                // [11..] = commitment data
                if calldata.len() < 11 {
                    return Err(anyhow!(
                        "Celestia batcher transaction too short: {} bytes, need at least 11",
                        calldata.len()
                    ));
                }

                // Check that the commitment type is altda (0x01).
                if calldata[1] != 0x01 {
                    return Err(anyhow!(
                        "Invalid commitment type for Celestia batcher transaction: expected 0x01, got 0x{:02x}",
                        calldata[1]
                    ));
                }

                // Check that the DA layer byte prefix is correct.
                // https://github.com/ethereum-optimism/specs/discussions/135.
                if calldata[2] != 0x0c {
                    return Err(anyhow!("Invalid prefix for Celestia batcher transaction"));
                }

                // The encoding of the commitment is the Celestia block height followed
                // by the Celestia commitment.
                let height_bytes = &calldata[3..11];
                let celestia_height = u64::from_le_bytes(
                    height_bytes
                        .try_into()
                        .map_err(|_| anyhow!("Failed to convert height bytes to u64"))?,
                );

                Ok(Some(celestia_height))
            }
            _ => {
                Err(anyhow!("Invalid version byte for batcher transaction: 0x{:02x}", calldata[0]))
            }
        }
    }
}

/// Get the latest Celestia block height that has been committed to Ethereum via Blobstream.
pub async fn get_latest_blobstream_celestia_block(fetcher: &OPSuccinctDataFetcher) -> Result<u64> {
    let blobstream_contract = SP1Blobstream::new(
        blobstream_address(fetcher.rollup_config.as_ref().unwrap().l1_chain_id)
            .expect("Failed to fetch blobstream contract address"),
        fetcher.l1_provider.clone(),
    );

    let latest_celestia_block = blobstream_contract.latestBlock().call().await?;
    Ok(latest_celestia_block)
}

fn is_valid_batch_transaction(
    tx: &EthTransaction,
    batch_inbox_address: Address,
    batcher_address: Address,
) -> Result<bool> {
    Ok(tx.to().is_some_and(|addr| addr == batch_inbox_address) &&
        tx.inner.recover_signer().is_ok_and(|signer| signer == batcher_address))
}

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

/// Find the latest safe L1 block with Celestia batches committed via Blobstream.
/// Uses binary search to efficiently locate the highest L1 block containing batch transactions
/// with Celestia heights that have been committed to Ethereum through Blobstream.
pub async fn get_celestia_safe_head_info(
    fetcher: &OPSuccinctDataFetcher,
    l2_reference_block: u64,
) -> Result<Option<CelestiaL1SafeHead>> {
    let rollup_config =
        fetcher.rollup_config.as_ref().ok_or_else(|| anyhow!("Rollup config not found"))?;

    let batch_inbox_address = rollup_config.batch_inbox_address;
    let batcher_address = rollup_config
        .genesis
        .system_config
        .as_ref()
        .ok_or_else(|| anyhow!("System config not found in genesis"))?
        .batcher_address;

    // Get the latest Celestia block committed via Blobstream.
    let latest_committed_celestia_block = get_latest_blobstream_celestia_block(fetcher).await?;

    // Get the L1 block range to search.
    let mut low = fetcher.get_safe_l1_block_for_l2_block(l2_reference_block).await?.1;
    let mut high = fetcher.get_l1_header(BlockId::finalized()).await?.number;
    let mut result = None;

    while low <= high {
        let current_l1_block = low + (high - low) / 2;
        let l1_block_hex = format!("0x{current_l1_block:x}");

        let safe_head_response: SafeHeadResponse = fetcher
            .fetch_rpc_data_with_mode(
                RPCMode::L2Node,
                "optimism_safeHeadAtL1Block",
                vec![l1_block_hex.into()],
            )
            .await?;

        let block = fetcher
            .l1_provider
            .get_block_by_number(BlockNumberOrTag::Number(safe_head_response.l1_block.number))
            .full()
            .await?
            .ok_or_else(|| anyhow!("Block {} not found", safe_head_response.l1_block.number))?;

        let mut found_valid_batch = false;
        for tx in block.transactions.txns() {
            if is_valid_batch_transaction(tx, batch_inbox_address, batcher_address)? {
                match extract_celestia_height(tx)? {
                    None => {
                        // ETH DA transaction - always valid.
                        found_valid_batch = true;
                        result = Some(CelestiaL1SafeHead {
                            l1_block_number: current_l1_block,
                            l2_safe_head_number: safe_head_response.safe_head.number,
                        });
                        break;
                    }
                    Some(celestia_height) => {
                        if celestia_height <= latest_committed_celestia_block {
                            found_valid_batch = true;
                            result = Some(CelestiaL1SafeHead {
                                l1_block_number: current_l1_block,
                                l2_safe_head_number: safe_head_response.safe_head.number,
                            });
                            break;
                        }
                    }
                }
            }
        }

        if found_valid_batch {
            low = current_l1_block + 1;
        } else {
            high = current_l1_block - 1;
        }
    }

    Ok(result)
}
