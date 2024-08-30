use alloy::{
    eips::BlockNumberOrTag,
    providers::{Provider, ProviderBuilder, RootProvider},
    transports::http::{reqwest::Url, Client, Http},
};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256};
use alloy_sol_types::SolValue;
use anyhow::Result;
use cargo_metadata::MetadataCommand;
use kona_host::HostCli;
use op_succinct_client_utils::RawBootInfo;
use std::{cmp::Ordering, env, fs, path::Path, str::FromStr, sync::Arc, time::Duration};
use tokio::time::sleep;

use alloy_primitives::keccak256;

use crate::{L2Output, ProgramType};

#[derive(Clone)]
/// The OPSuccinctDataFetcher struct is used to fetch the L2 output data and L2 claim data for a
/// given block number. It is used to generate the boot info for the native host program.
pub struct OPSuccinctDataFetcher {
    pub l1_rpc: String,
    pub l1_provider: Arc<RootProvider<Http<Client>>>,
    pub l1_beacon_rpc: String,
    pub l2_rpc: String,
    pub l2_node_rpc: String,
    pub l2_provider: Arc<RootProvider<Http<Client>>>,
}

impl Default for OPSuccinctDataFetcher {
    fn default() -> Self {
        OPSuccinctDataFetcher::new()
    }
}

/// The mode corresponding to the chain we are fetching data for.
#[derive(Clone, Copy)]
pub enum ChainMode {
    L1,
    L2,
}

/// The info to fetch for a block.
pub struct BlockInfo {
    pub block_number: u64,
    pub transaction_count: u64,
    pub gas_used: u64,
}

impl OPSuccinctDataFetcher {
    pub fn new() -> Self {
        dotenv::dotenv().ok();
        let l1_rpc = env::var("L1_RPC").unwrap_or_else(|_| "http://localhost:8545".to_string());
        let l1_provider =
            Arc::new(ProviderBuilder::default().on_http(Url::from_str(&l1_rpc).unwrap()));
        let l1_beacon_rpc =
            env::var("L1_BEACON_RPC").unwrap_or_else(|_| "http://localhost:5052".to_string());
        let l2_rpc = env::var("L2_RPC").unwrap_or_else(|_| "http://localhost:9545".to_string());
        let l2_node_rpc =
            env::var("L2_NODE_RPC").unwrap_or_else(|_| "http://localhost:5058".to_string());
        let l2_provider =
            Arc::new(ProviderBuilder::default().on_http(Url::from_str(&l2_rpc).unwrap()));
        OPSuccinctDataFetcher {
            l1_rpc,
            l1_provider,
            l1_beacon_rpc,
            l2_rpc,
            l2_node_rpc,
            l2_provider,
        }
    }

    pub fn get_provider(&self, chain_mode: ChainMode) -> Arc<RootProvider<Http<Client>>> {
        match chain_mode {
            ChainMode::L1 => self.l1_provider.clone(),
            ChainMode::L2 => self.l2_provider.clone(),
        }
    }

    /// Get the earliest L1 header in a batch of boot infos.
    pub async fn get_earliest_l1_head_in_batch(
        &self,
        boot_infos: &[RawBootInfo],
    ) -> Result<Header> {
        let mut earliest_block_num: u64 = u64::MAX;
        let mut earliest_l1_header: Option<Header> = None;

        for boot_info in boot_infos {
            let l1_block_header = self.get_header_by_hash(ChainMode::L1, boot_info.l1_head).await?;
            if l1_block_header.number < earliest_block_num {
                earliest_block_num = l1_block_header.number;
                earliest_l1_header = Some(l1_block_header);
            }
        }
        Ok(earliest_l1_header.unwrap())
    }

    /// Fetch headers for a range of blocks inclusive.
    pub async fn fetch_headers_in_range(&self, start: u64, end: u64) -> Result<Vec<Header>> {
        let mut headers: Vec<Header> = Vec::with_capacity((end - start + 1) as usize);

        // Note: Node rate limits at 300 requests per second.
        let batch_size = 200;
        let mut block_number = start;
        while block_number <= end {
            let batch_end = block_number + batch_size - 1;
            let batch_headers: Vec<Header> = futures::future::join_all(
                (block_number..=batch_end.min(end))
                    .map(|num| self.get_header_by_number(ChainMode::L1, num)),
            )
            .await
            .into_iter()
            .map(|header| header.unwrap())
            .collect();

            headers.extend(batch_headers);
            block_number += batch_size;
            sleep(Duration::from_millis(1500)).await;
        }
        Ok(headers)
    }

    /// Get the preimages for the headers corresponding to the boot infos. Specifically, fetch the
    /// headers corresponding to the boot infos and the latest L1 head.
    pub async fn get_header_preimages(
        &self,
        boot_infos: &[RawBootInfo],
        checkpoint_block_hash: B256,
    ) -> Result<Vec<Header>> {
        // Get the earliest L1 Head from the boot_infos.
        let start_header = self.get_earliest_l1_head_in_batch(boot_infos).await?;

        // Fetch the full header for the latest L1 Head (which is validated on chain).
        let latest_header = self.get_header_by_hash(ChainMode::L1, checkpoint_block_hash).await?;

        // Create a vector of futures for fetching all headers
        let headers =
            self.fetch_headers_in_range(start_header.number, latest_header.number).await?;

        Ok(headers)
    }

    pub async fn get_header_by_hash(
        &self,
        chain_mode: ChainMode,
        block_hash: B256,
    ) -> Result<Header> {
        let provider = self.get_provider(chain_mode);
        let header = provider
            .get_block_by_hash(block_hash, alloy::rpc::types::BlockTransactionsKind::Full)
            .await?
            .unwrap()
            .header;
        Ok(header.try_into().unwrap())
    }

    pub async fn get_chain_id(&self, chain_mode: ChainMode) -> Result<u64> {
        let provider = self.get_provider(chain_mode);
        let chain_id = provider.get_chain_id().await?;
        Ok(chain_id)
    }

    pub async fn get_head(&self, chain_mode: ChainMode) -> Result<Header> {
        let provider = self.get_provider(chain_mode);
        let header =
            provider.get_block_by_number(BlockNumberOrTag::Latest, false).await?.unwrap().header;
        Ok(header.try_into().unwrap())
    }

    pub async fn get_header_by_number(
        &self,
        chain_mode: ChainMode,
        block_number: u64,
    ) -> Result<Header> {
        let provider = self.get_provider(chain_mode);
        let header =
            provider.get_block_by_number(block_number.into(), false).await?.unwrap().header;
        Ok(header.try_into().unwrap())
    }

    /// Get the block data for a range of blocks inclusive.
    pub async fn get_block_data_range(
        &self,
        chain_mode: ChainMode,
        start: u64,
        end: u64,
    ) -> Result<Vec<BlockInfo>> {
        let mut block_data = Vec::new();
        for block_number in start..=end {
            let provider = self.get_provider(chain_mode);
            let block = provider.get_block_by_number(block_number.into(), false).await?.unwrap();
            block_data.push(BlockInfo {
                block_number,
                transaction_count: block.transactions.len() as u64,
                gas_used: block.header.gas_used as u64,
            });
        }
        Ok(block_data)
    }

    /// Find the block with the closest timestamp to the target timestamp.
    async fn find_block_by_timestamp(
        &self,
        chain_mode: ChainMode,
        target_timestamp: u64,
    ) -> Result<B256> {
        let provider = self.get_provider(chain_mode);
        let latest_block =
            provider.get_block_by_number(BlockNumberOrTag::Latest, false).await?.unwrap();
        let mut low = 0;
        let mut high = latest_block.header.number.unwrap();

        while low <= high {
            let mid = (low + high) / 2;
            let block = provider.get_block_by_number(mid.into(), false).await?.unwrap();
            let block_timestamp = block.header.timestamp;

            match block_timestamp.cmp(&target_timestamp) {
                Ordering::Equal => return Ok(block.header.hash.unwrap().0.into()),
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid - 1,
            }
        }

        // Return the block hash of the closest block after the target timestamp
        let block = provider.get_block_by_number(low.into(), false).await?.unwrap();
        Ok(block.header.hash.unwrap().0.into())
    }

    /// Get the L2 output data for a given block number and save the boot info to a file in the data
    /// directory with block_number. Return the arguments to be passed to the native host for
    /// datagen.
    pub async fn get_host_cli_args(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        multi_block: ProgramType,
    ) -> Result<HostCli> {
        if l2_start_block >= l2_end_block {
            return Err(anyhow::anyhow!("L2 start block is greater than or equal to L2 end block"));
        }

        let l2_provider = self.l2_provider.clone();

        // Get L2 output data.
        let l2_output_block =
            l2_provider.get_block_by_number(l2_start_block.into(), false).await?.unwrap();
        let l2_output_state_root = l2_output_block.header.state_root;
        let l2_head = l2_output_block.header.hash.expect("L2 head is missing");
        let l2_output_storage_hash = l2_provider
            .get_proof(Address::from_str("0x4200000000000000000000000000000000000016")?, Vec::new())
            .block_id(l2_start_block.into())
            .await?
            .storage_hash;

        let l2_output_encoded = L2Output {
            zero: 0,
            l2_state_root: l2_output_state_root.0.into(),
            l2_storage_hash: l2_output_storage_hash.0.into(),
            l2_claim_hash: l2_head.0.into(),
        };
        let l2_output_root = keccak256(l2_output_encoded.abi_encode());

        // Get L2 claim data.
        let l2_claim_block =
            l2_provider.get_block_by_number(l2_end_block.into(), false).await?.unwrap();
        let l2_claim_state_root = l2_claim_block.header.state_root;
        let l2_claim_hash = l2_claim_block.header.hash.expect("L2 claim hash is missing");
        let l2_claim_storage_hash = l2_provider
            .get_proof(Address::from_str("0x4200000000000000000000000000000000000016")?, Vec::new())
            .block_id(l2_end_block.into())
            .await?
            .storage_hash;

        let l2_claim_encoded = L2Output {
            zero: 0,
            l2_state_root: l2_claim_state_root.0.into(),
            l2_storage_hash: l2_claim_storage_hash.0.into(),
            l2_claim_hash: l2_claim_hash.0.into(),
        };
        let l2_claim = keccak256(l2_claim_encoded.abi_encode());

        // Get L1 head.
        let l2_block_timestamp = l2_claim_block.header.timestamp;
        // Note: This limit is set so that the l1 head is always ahead of the l2 claim block.
        // E.g. Origin Advance Error: BlockInfoFetch(Block number past L1 head.)
        let target_timestamp = l2_block_timestamp + 600;
        let l1_head = self.find_block_by_timestamp(ChainMode::L1, target_timestamp).await?;

        // Get the chain id.
        let l2_chain_id = l2_provider.get_chain_id().await?;

        // Get the workspace root, which is where the data directory is.
        let metadata = MetadataCommand::new().exec().unwrap();
        let workspace_root = metadata.workspace_root;
        let data_directory = match multi_block {
            ProgramType::Single => {
                let proof_dir =
                    format!("{}/data/{}/single/{}", workspace_root, l2_chain_id, l2_end_block);
                proof_dir
            }
            ProgramType::Multi => {
                let proof_dir = format!(
                    "{}/data/{}/multi/{}-{}",
                    workspace_root, l2_chain_id, l2_start_block, l2_end_block
                );
                proof_dir
            }
        };

        // The native programs are built with profile release-client-lto in build.rs
        let exec_directory = match multi_block {
            ProgramType::Single => {
                format!("{}/target/release-client-lto/fault-proof", workspace_root)
            }
            ProgramType::Multi => format!("{}/target/release-client-lto/range", workspace_root),
        };

        // Create data directory. This will be used by the host program running in native execution
        // mode to save all preimages.
        if !Path::new(&data_directory).exists() {
            fs::create_dir_all(&data_directory)?;
        }

        Ok(HostCli {
            l1_head: l1_head.0.into(),
            l2_output_root: l2_output_root.0.into(),
            l2_claim: l2_claim.0.into(),
            l2_block_number: l2_end_block,
            l2_chain_id,
            l2_head: l2_head.0.into(),
            l2_node_address: Some(self.l2_rpc.clone()),
            l1_node_address: Some(self.l1_rpc.clone()),
            l1_beacon_address: Some(self.l1_beacon_rpc.clone()),
            data_dir: Some(data_directory.into()),
            exec: Some(exec_directory),
            server: false,
            rollup_config_path: None,
            v: 0,
        })
    }
}
