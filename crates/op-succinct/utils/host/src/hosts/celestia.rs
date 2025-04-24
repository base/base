use alloy_consensus::Transaction;
use alloy_eips::BlockId;
use alloy_primitives::{address, Address, B256};
use alloy_provider::Provider;
use async_trait::async_trait;
use hana_host::celestia::{CelestiaCfg, CelestiaChainHost};
use kona_preimage::BidirectionalChannel;
use kona_rpc::SafeHeadResponse;
use op_succinct_client_utils::witness::WitnessData;
use std::sync::Arc;

use crate::{
    fetcher::{OPSuccinctDataFetcher, RPCMode},
    hosts::OPSuccinctHost,
    SP1Blobstream,
};
use anyhow::{anyhow, Result};

#[derive(Clone)]
pub struct CelestiaOPSuccinctHost {
    pub fetcher: Arc<OPSuccinctDataFetcher>,
}

#[async_trait]
impl OPSuccinctHost for CelestiaOPSuccinctHost {
    type Args = CelestiaChainHost;

    async fn run(&self, args: &Self::Args) -> Result<WitnessData> {
        let hint = BidirectionalChannel::new()?;
        let preimage = BidirectionalChannel::new()?;

        let server_task = args.start_server(hint.host, preimage.host).await?;

        let witness = Self::run_witnessgen_client(preimage.client, hint.client).await?;
        // Unlike the upstream, manually abort the server task, as it will hang if you wait for both
        // tasks to complete.
        server_task.abort();

        Ok(witness)
    }

    async fn fetch(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        l1_head_hash: Option<B256>,
        safe_db_fallback: Option<bool>,
    ) -> Result<CelestiaChainHost> {
        let host = self
            .fetcher
            .get_host_args(
                l2_start_block,
                l2_end_block,
                l1_head_hash,
                safe_db_fallback.expect("`safe_db_fallback` must be set"),
            )
            .await?;

        // Create `CelestiaCfg` directly from environment variables
        let celestia_args = CelestiaCfg {
            celestia_connection: std::env::var("CELESTIA_CONNECTION").ok(),
            auth_token: std::env::var("AUTH_TOKEN").ok(),
            namespace: std::env::var("NAMESPACE").ok(),
        };

        Ok(CelestiaChainHost { single_host: host, celestia_args })
    }

    fn get_l1_head_hash(&self, args: &Self::Args) -> Option<B256> {
        Some(args.single_host.l1_head)
    }

    /// Converts the latest Celestia block height in Blobstream to the highest L2 block that can be
    /// included in a range proof.
    ///
    /// 1. Get the latest Celestia block included in a Blobstream commitment.
    /// 2. Loop over the `BatchInbox` from the l1 origin of the latest proposed block number to the
    ///    finalized L1 block number.
    /// 3. For each `BatchInbox` transaction, check if it contains a Celestia block number greater
    ///    than the latest Celestia block.
    /// 4. If it does, return the L2 block number.
    /// 5. If it doesn't, return None.
    async fn get_finalized_l2_block_number(
        &self,
        fetcher: &OPSuccinctDataFetcher,
        latest_proposed_block_number: u64,
    ) -> Result<Option<u64>> {
        let batch_inbox_address = fetcher.rollup_config.as_ref().unwrap().batch_inbox_address;

        let blobstream_contract = SP1Blobstream::new(
            get_blobstream_address(fetcher.rollup_config.as_ref().unwrap().l1_chain_id),
            fetcher.l1_provider.clone(),
        );
        // Get the latest Celestia block included in a Blobstream commitment.
        let latest_celestia_block = blobstream_contract.latestBlock().call().await?.latestBlock;

        let mut low = fetcher.get_safe_l1_block_for_l2_block(latest_proposed_block_number).await?.1;
        let mut high = fetcher.get_l1_header(BlockId::finalized()).await?.number;

        let mut l2_block_number = None;

        // Binary search between the latest proposed block number and the finalized L1 block number
        // for the batch transaction that has the highest Celestia height less than the
        // latest Celestia height in the latest Blobstream commitment.
        //
        // At each block in the binary search, get the current safe head (this returns the L1 block
        // where the batch was posted). Then, get the block at the safe head and check if it
        // contains a batch transaction with a Celestia height greater than the latest Celestia
        // height in the latest Blobstream commitment.
        while low <= high {
            let mid = (high + low) / 2;
            let l1_block_hex = format!("0x{mid:x}");

            // Get the safe head for the chain at the midpoint. This will return the latest
            // transaction with a batch.
            let result: SafeHeadResponse = fetcher
                .fetch_rpc_data_with_mode(
                    RPCMode::L2Node,
                    "optimism_safeHeadAtL1Block",
                    vec![l1_block_hex.into()],
                )
                .await?;
            let safe_head_l1_block_number = result.l1_block.number;
            let l2_safe_head_number = result.safe_head.number;
            let block = fetcher
                .l1_provider
                .get_block_by_number(alloy_eips::BlockNumberOrTag::Number(
                    safe_head_l1_block_number,
                ))
                .full()
                .await?
                .unwrap();

            let mut found_valid_tx = false;
            for tx in block.transactions.txns() {
                if let Some(to_addr) = tx.to() {
                    if to_addr == batch_inbox_address {
                        match extract_celestia_height(tx)? {
                            None => {
                                // ETH DA transaction.
                                found_valid_tx = true;
                                l2_block_number =
                                    Some(fetcher.get_l2_header(BlockId::finalized()).await?.number);
                            }
                            Some(celestia_height) => {
                                if celestia_height < latest_celestia_block {
                                    found_valid_tx = true;
                                    l2_block_number = Some(l2_safe_head_number);
                                }
                            }
                        }
                    }
                }
            }

            // If a batch b with a lower Celestia height, h1, than the latest Blobstream Celestia
            // height, h, in the latest Blobstream commitment was found, we should try
            // to find a batch b' with a Celestia height, h2, that is h1 < h2 <= h.
            if found_valid_tx {
                low = mid + 1; // Look in higher blocks for a batch with a higher Celestia height
                               // that's less than the latest Blobstream Celestia height.
            } else {
                high = mid - 1; // The Celestia height in the latest committed batch is greater than
                                // the latest Blobstream Celestia height, so look in earlier blocks.
            }
        }

        Ok(l2_block_number)
    }
}

impl CelestiaOPSuccinctHost {
    pub fn new(fetcher: Arc<OPSuccinctDataFetcher>) -> Self {
        Self { fetcher }
    }
}

/// Get the Blobstream contract address for a given L1 chain ID.
///
/// The addresses can be found here: https://docs.celestia.org/how-to-guides/blobstream
fn get_blobstream_address(l1_chain_id: u64) -> Address {
    match l1_chain_id {
        1 => address!("7Cf3876F681Dbb6EdA8f6FfC45D66B996Df08fAe"),
        42161 => address!("A83ca7775Bc2889825BcDeDfFa5b758cf69e8794"),
        8453 => address!("A83ca7775Bc2889825BcDeDfFa5b758cf69e8794"),
        11155111 => address!("f0c6429ebab2e7dc6e05dafb61128be21f13cb1e"),
        421614 => address!("c3e209eb245Fd59c8586777b499d6A665DF3ABD2"),
        84532 => address!("c3e209eb245Fd59c8586777b499d6A665DF3ABD2"),
        _ => panic!("Unsupported L1 chain ID: {l1_chain_id}"),
    }
}

/// Extract the Celestia height from batcher transaction based on the version byte.
///
/// Returns:
/// - Some(height) if the transaction is a valid Celestia batcher transaction.
/// - None if the transaction is an ETH DA transaction (EIP4844 transaction or non-EIP4844
///   transaction with version byte 0x00).
/// - Err if the version byte is invalid or the da layer byte is incorrect for non-EIP4844
///   transactions.
fn extract_celestia_height(tx: &alloy_rpc_types::eth::Transaction) -> Result<Option<u64>> {
    // Skip calldata parsing for EIP4844 transactions since there is no calldata.
    if tx.inner.is_eip4844() {
        Ok(None)
    } else {
        let calldata = tx.input();
        // Check version byte to determine if it is ETH DA or Alt DA.
        // https://specs.optimism.io/protocol/derivation.html#batcher-transaction-format.
        match calldata[0] {
            0x00 => Ok(None), // ETH DA transaction.
            0x01 => {
                // Check that the DA layer byte prefix is correct.
                // https://github.com/ethereum-optimism/specs/discussions/135.
                if calldata[2] != 0x0c {
                    return Err(anyhow!("Invalid prefix for Celestia batcher transaction"));
                }

                // The encoding of the commitment is the Celestia block height followed
                // by the Celestia commitment.
                let height_bytes = &calldata[3..11];
                let celestia_height = u64::from_le_bytes(height_bytes.try_into().unwrap());

                Ok(Some(celestia_height))
            }
            _ => Err(anyhow!("Invalid version byte for batcher transaction")),
        }
    }
}
