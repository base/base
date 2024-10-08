use alloy::{
    eips::BlockNumberOrTag,
    primitives::{Address, B256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::Block,
    transports::http::{reqwest::Url, Client, Http},
};
use alloy_consensus::Header;
use alloy_sol_types::SolValue;
use anyhow::Result;
use cargo_metadata::MetadataCommand;
use kona_host::HostCli;
use op_alloy_genesis::RollupConfig;
use op_alloy_rpc_types::output::OutputResponse;
use op_succinct_client_utils::boot::BootInfoStruct;
use serde_json::{json, Value};
use sp1_sdk::block_on;
use std::{cmp::Ordering, env, fs, path::Path, str::FromStr, sync::Arc, time::Duration};
use tokio::time::sleep;

use alloy_primitives::keccak256;

use crate::{
    rollup_config::{get_rollup_config_path, merge_rollup_config, save_rollup_config},
    L2Output, ProgramType,
};

#[derive(Clone)]
/// The OPSuccinctDataFetcher struct is used to fetch the L2 output data and L2 claim data for a
/// given block number. It is used to generate the boot info for the native host program.
/// TODO: Add retries for all requests (3 retries).
pub struct OPSuccinctDataFetcher {
    pub rpc_config: RPCConfig,
    pub l1_provider: Arc<RootProvider<Http<Client>>>,
    pub l2_provider: Arc<RootProvider<Http<Client>>>,
    pub rollup_config: RollupConfig,
    pub l1_block_time_secs: u64,
}

impl Default for OPSuccinctDataFetcher {
    fn default() -> Self {
        block_on(OPSuccinctDataFetcher::new())
    }
}

#[derive(Debug, Clone)]
pub struct RPCConfig {
    l1_rpc: String,
    l1_beacon_rpc: String,
    l2_rpc: String,
    l2_node_rpc: String,
}

/// The mode corresponding to the chain we are fetching data for.
#[derive(Clone, Copy)]
pub enum RPCMode {
    L1,
    L1Beacon,
    L2,
    L2Node,
}

/// Whether to keep the cache or delete the cache.
#[derive(Clone, Copy)]
pub enum CacheMode {
    KeepCache,
    DeleteCache,
}

fn get_rpcs() -> RPCConfig {
    RPCConfig {
        l1_rpc: env::var("L1_RPC").unwrap_or_else(|_| "http://localhost:8545".to_string()),
        l1_beacon_rpc: env::var("L1_BEACON_RPC")
            .unwrap_or_else(|_| "http://localhost:5052".to_string()),
        l2_rpc: env::var("L2_RPC").unwrap_or_else(|_| "http://localhost:9545".to_string()),
        l2_node_rpc: env::var("L2_NODE_RPC")
            .unwrap_or_else(|_| "http://localhost:5058".to_string()),
    }
}

/// The info to fetch for a block.
pub struct BlockInfo {
    pub block_number: u64,
    pub transaction_count: u64,
    pub gas_used: u64,
}

impl OPSuccinctDataFetcher {
    /// Gets the RPC URL's and saves the rollup config for the chain to the rollup config file.
    pub async fn new() -> Self {
        let rpc_config = get_rpcs();

        let l1_provider = Arc::new(
            ProviderBuilder::default().on_http(Url::from_str(&rpc_config.l1_rpc).unwrap()),
        );
        let l2_provider = Arc::new(
            ProviderBuilder::default().on_http(Url::from_str(&rpc_config.l2_rpc).unwrap()),
        );

        let mut fetcher = OPSuccinctDataFetcher {
            rpc_config,
            l1_provider,
            l2_provider,
            rollup_config: RollupConfig::default(),
            // Default L1 block time for most Ethereum chains.
            l1_block_time_secs: 12,
        };

        // Get the L1 block time.
        let l1_block_time_secs = fetcher
            .get_l1_block_time()
            .await
            .expect("Failed to get L1 block time. Make sure that the L1 RPC is active.");
        fetcher.l1_block_time_secs = l1_block_time_secs;

        // Load and save the rollup config.
        let rollup_config = fetcher
            .fetch_rollup_config()
            .await
            .expect("Failed to fetch rollup config");
        save_rollup_config(&rollup_config).expect("Failed to save rollup config");
        fetcher.rollup_config = rollup_config;

        fetcher
    }

    /// Get the RPC URL for the given RPC mode.
    pub fn get_rpc_url(&self, rpc_mode: RPCMode) -> String {
        match rpc_mode {
            RPCMode::L1 => self.rpc_config.l1_rpc.clone(),
            RPCMode::L2 => self.rpc_config.l2_rpc.clone(),
            RPCMode::L1Beacon => self.rpc_config.l1_beacon_rpc.clone(),
            RPCMode::L2Node => self.rpc_config.l2_node_rpc.clone(),
        }
    }

    /// Get the provider for the given RPC mode. Note: Will panic if the RPC mode is not L1 or L2.
    /// Note: The provider can be dropped by the Tokio runtime if it is not used for a long time. Be
    /// careful when using this function.
    pub fn get_provider(&self, rpc_mode: RPCMode) -> Arc<RootProvider<Http<Client>>> {
        match rpc_mode {
            RPCMode::L1 => self.l1_provider.clone(),
            RPCMode::L2 => self.l2_provider.clone(),
            RPCMode::L1Beacon | RPCMode::L2Node => {
                panic!("L1Beacon and L2Node modes do not have associated providers")
            }
        }
    }

    /// Fetch the rollup config. Combines the rollup config from `optimism_rollupConfig` and the
    /// chain config from `debug_chainConfig`.
    pub async fn fetch_rollup_config(&self) -> Result<RollupConfig> {
        let rollup_config = self
            .fetch_rpc_data(RPCMode::L2Node, "optimism_rollupConfig", vec![])
            .await?;
        let chain_config = self
            .fetch_rpc_data(RPCMode::L2, "debug_chainConfig", vec![])
            .await?;
        merge_rollup_config(&rollup_config, &chain_config)
    }

    /// Fetch arbitrary data from the RPC.
    pub async fn fetch_rpc_data<T>(
        &self,
        rpc_mode: RPCMode,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let client = reqwest::Client::new();
        let response = client
            .post(self.get_rpc_url(rpc_mode))
            .json(&json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
                "id": 1
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        // Check for RPC error from the JSON RPC response.
        if let Some(error) = response.get("error") {
            let error_message = error["message"].as_str().unwrap_or("Unknown error");
            return Err(anyhow::anyhow!("RPC error: {}", error_message));
        }

        serde_json::from_value(response["result"].clone()).map_err(Into::into)
    }

    /// Get the earliest L1 header in a batch of boot infos.
    pub async fn get_earliest_l1_head_in_batch(
        &self,
        boot_infos: &Vec<BootInfoStruct>,
    ) -> Result<Header> {
        let mut earliest_block_num: u64 = u64::MAX;
        let mut earliest_l1_header: Option<Header> = None;

        for boot_info in boot_infos {
            let l1_block_header = self
                .get_header_by_hash(RPCMode::L1, boot_info.l1Head)
                .await?;
            if l1_block_header.number < earliest_block_num {
                earliest_block_num = l1_block_header.number;
                earliest_l1_header = Some(l1_block_header);
            }
        }
        Ok(earliest_l1_header.unwrap())
    }

    /// Fetch headers for a range of blocks inclusive.
    pub async fn fetch_headers_in_range(&self, start: u64, end: u64) -> Result<Vec<Header>> {
        let mut headers: Vec<Header> = Vec::with_capacity((end - start + 1).try_into().unwrap());

        // Note: Node rate limits at 300 requests per second.
        let batch_size = 200;
        let mut block_number = start;
        while block_number <= end {
            let batch_end = block_number + batch_size - 1;
            let batch_headers: Vec<Header> = futures::future::join_all(
                (block_number..=batch_end.min(end))
                    .map(|num| self.get_header_by_number(RPCMode::L1, num)),
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
        boot_infos: &Vec<BootInfoStruct>,
        checkpoint_block_hash: B256,
    ) -> Result<Vec<Header>> {
        // Get the earliest L1 Head from the boot_infos.
        let start_header = self.get_earliest_l1_head_in_batch(boot_infos).await?;

        // Fetch the full header for the latest L1 Head (which is validated on chain).
        let latest_header = self
            .get_header_by_hash(RPCMode::L1, checkpoint_block_hash)
            .await?;

        // Create a vector of futures for fetching all headers
        let headers = self
            .fetch_headers_in_range(start_header.number, latest_header.number)
            .await?;

        Ok(headers)
    }

    pub async fn get_header_by_hash(&self, rpc_mode: RPCMode, block_hash: B256) -> Result<Header> {
        let provider = self.get_provider(rpc_mode);
        let header = provider
            .get_block_by_hash(block_hash, alloy::rpc::types::BlockTransactionsKind::Full)
            .await?
            .unwrap()
            .header;
        Ok(header.try_into().unwrap())
    }

    pub async fn get_chain_id(&self, rpc_mode: RPCMode) -> Result<u64> {
        let provider = self.get_provider(rpc_mode);
        let chain_id = provider.get_chain_id().await?;
        Ok(chain_id)
    }

    pub async fn get_head(&self, rpc_mode: RPCMode) -> Result<Header> {
        let provider = self.get_provider(rpc_mode);
        let header = provider
            .get_block_by_number(BlockNumberOrTag::Latest, false)
            .await?
            .unwrap()
            .header;
        Ok(header.try_into().unwrap())
    }

    pub async fn get_header_by_number(
        &self,
        rpc_mode: RPCMode,
        block_number: u64,
    ) -> Result<Header> {
        let provider = self.get_provider(rpc_mode);
        let header = provider
            .get_block_by_number(block_number.into(), false)
            .await?
            .unwrap()
            .header;
        Ok(header.try_into().unwrap())
    }

    /// Get the block data for a range of blocks inclusive.
    pub async fn get_block_data_range(
        &self,
        rpc_mode: RPCMode,
        start: u64,
        end: u64,
    ) -> Result<Vec<BlockInfo>> {
        let mut block_data = Vec::new();
        for block_number in start..=end {
            let provider = self.get_provider(rpc_mode);
            let block = provider
                .get_block_by_number(block_number.into(), false)
                .await?
                .unwrap();
            block_data.push(BlockInfo {
                block_number,
                transaction_count: block.transactions.len() as u64,
                gas_used: block.header.gas_used,
            });
        }
        Ok(block_data)
    }

    pub async fn get_block_by_number(&self, rpc_mode: RPCMode, block_number: u64) -> Result<Block> {
        let provider = self.get_provider(rpc_mode);
        let block = provider
            .get_block_by_number(block_number.into(), false)
            .await?
            .unwrap();
        Ok(block)
    }

    /// Find the block with the closest timestamp to the target timestamp.
    async fn find_block_hash_by_timestamp(
        &self,
        rpc_mode: RPCMode,
        target_timestamp: u64,
    ) -> Result<B256> {
        let provider = self.get_provider(rpc_mode);
        let latest_block = provider
            .get_block_by_number(BlockNumberOrTag::Latest, false)
            .await?
            .unwrap();
        let mut low = 0;
        let mut high = latest_block.header.number;

        while low <= high {
            let mid = (low + high) / 2;
            let block = provider
                .get_block_by_number(mid.into(), false)
                .await?
                .unwrap();
            let block_timestamp = block.header.timestamp;

            match block_timestamp.cmp(&target_timestamp) {
                Ordering::Equal => return Ok(block.header.hash.0.into()),
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid - 1,
            }
        }

        // Return the block hash of the closest block after the target timestamp
        let block = provider
            .get_block_by_number(low.into(), false)
            .await?
            .unwrap();
        Ok(block.header.hash.0.into())
    }

    /// Get the L2 output data for a given block number and save the boot info to a file in the data
    /// directory with block_number. Return the arguments to be passed to the native host for
    /// datagen.
    pub async fn get_host_cli_args(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        multi_block: ProgramType,
        cache_mode: CacheMode,
    ) -> Result<HostCli> {
        if l2_start_block >= l2_end_block {
            return Err(anyhow::anyhow!(
                "L2 start block is greater than or equal to L2 end block. Start: {}, End: {}",
                l2_start_block,
                l2_end_block
            ));
        }

        let l2_provider = self.l2_provider.clone();

        // Get L2 output data.
        let l2_output_block = l2_provider
            .get_block_by_number(l2_start_block.into(), false)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("Block not found for block number {}", l2_start_block)
            })?;
        let l2_output_state_root = l2_output_block.header.state_root;
        let l2_head = l2_output_block.header.hash;
        let l2_output_storage_hash = l2_provider
            .get_proof(
                Address::from_str("0x4200000000000000000000000000000000000016")?,
                Vec::new(),
            )
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
        let l2_claim_block = l2_provider
            .get_block_by_number(l2_end_block.into(), false)
            .await?
            .unwrap();
        let l2_claim_state_root = l2_claim_block.header.state_root;
        let l2_claim_hash = l2_claim_block.header.hash;
        let l2_claim_storage_hash = l2_provider
            .get_proof(
                Address::from_str("0x4200000000000000000000000000000000000016")?,
                Vec::new(),
            )
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

        let l1_head = self.get_l1_head(l2_end_block).await?;

        // Get the workspace root, which is where the data directory is.
        let metadata = MetadataCommand::new().exec().unwrap();
        let workspace_root = metadata.workspace_root;
        let data_directory = match multi_block {
            ProgramType::Single => {
                let proof_dir = format!(
                    "{}/data/{}/single/{}",
                    workspace_root, self.rollup_config.l2_chain_id, l2_end_block
                );
                proof_dir
            }
            ProgramType::Multi => {
                let proof_dir = format!(
                    "{}/data/{}/multi/{}-{}",
                    workspace_root, self.rollup_config.l2_chain_id, l2_start_block, l2_end_block
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

        // Delete the data directory if the cache mode is DeleteCache.
        match cache_mode {
            CacheMode::KeepCache => (),
            CacheMode::DeleteCache => {
                if Path::new(&data_directory).exists() {
                    fs::remove_dir_all(&data_directory)?;
                }
            }
        }

        // Create the path to the rollup config file.
        let rollup_config_path = get_rollup_config_path(self.rollup_config.l2_chain_id)?;

        // Creates the data directory if it doesn't exist, or no-ops if it does. Used to store the
        // witness data.
        fs::create_dir_all(&data_directory)?;

        Ok(HostCli {
            l1_head,
            l2_output_root,
            l2_claim,
            l2_block_number: l2_end_block,
            l2_chain_id: None,
            l2_head,
            l2_node_address: Some(self.rpc_config.l2_rpc.clone()),
            l1_node_address: Some(self.rpc_config.l1_rpc.clone()),
            l1_beacon_address: Some(self.rpc_config.l1_beacon_rpc.clone()),
            data_dir: Some(data_directory.into()),
            exec: Some(exec_directory),
            server: false,
            rollup_config_path: Some(rollup_config_path),
            v: std::env::var("VERBOSITY")
                .unwrap_or("0".to_string())
                .parse()
                .unwrap(),
        })
    }

    /// Get the L1 block time in seconds.
    async fn get_l1_block_time(&self) -> Result<u64> {
        let l1_provider = self.l1_provider.clone();
        let l1_head = self.get_head(RPCMode::L1).await?;

        let l1_head_minus_1 = l1_head.number - 1;
        let l1_block_minus_1 = l1_provider
            .get_block_by_number(l1_head_minus_1.into(), false)
            .await?
            .unwrap();
        Ok(l1_head.timestamp - l1_block_minus_1.header.timestamp)
    }

    // TODO: Use op-alloy types.
    /// Get the L1 block from which the `l2_end_block` can be derived.
    async fn get_l1_head_with_safe_head(&self, l2_end_block: u64) -> Result<B256> {
        let latest_l1_header = self.get_head(RPCMode::L1).await?;

        // Get the l1 origin of the l2 end block.
        let l2_end_block_hex = format!("0x{:x}", l2_end_block);
        let optimism_output_data: OutputResponse = self
            .fetch_rpc_data(
                RPCMode::L2Node,
                "optimism_outputAtBlock",
                vec![l2_end_block_hex.into()],
            )
            .await?;

        let l1_origin = optimism_output_data.block_ref.l1_origin;

        // Search forward from the l1Origin, skipping forward in 5 minute increments until an L1 block with an L2 safe head greater than the l2_end_block is found.
        let mut current_l1_block_number = l1_origin.number;
        loop {
            // If the current L1 block number is greater than the latest L1 header number, then return an error.
            if current_l1_block_number > latest_l1_header.number {
                return Err(anyhow::anyhow!(
                    "Could not find an L1 block with an L2 safe head greater than the L2 end block."
                ));
            }

            let l1_block_number_hex = format!("0x{:x}", current_l1_block_number);
            // TODO: Use op-alloy types once the bug for safeHeadResponse is fixed: https://github.com/alloy-rs/op-alloy/issues/155
            let result: Value = self
                .fetch_rpc_data(
                    RPCMode::L2Node,
                    "optimism_safeHeadAtL1Block",
                    vec![l1_block_number_hex.into()],
                )
                .await?;
            let l2_safe_head = result["safeHead"]["number"].as_u64().unwrap();
            if l2_safe_head > l2_end_block {
                return Ok(B256::from_str(result["l1Block"]["hash"].as_str().unwrap()).unwrap());
            }

            // Move forward in 5 minute increments.
            const SKIP_MINS: u64 = 5;
            current_l1_block_number += SKIP_MINS * (60 / self.l1_block_time_secs);
        }
    }

    /// For OP Sepolia, OP Mainnet and Base, the batcher posts at least every 10 minutes. Otherwise,
    /// the batcher may post as infrequently as every couple hours. The l1Head is set as the l1 block from which all of the
    /// relevant L2 block data can be derived.
    /// E.g. Origin Advance Error: BlockInfoFetch(Block number past L1 head.).
    async fn get_l1_head(&self, l2_end_block: u64) -> Result<B256> {
        // See if optimism_safeHeadAtL1Block is available. If there's an error, then estimate the L1 block necessary based on the chain config.
        let result = self.get_l1_head_with_safe_head(l2_end_block).await;

        if let Ok(safe_head_at_l1_block) = result {
            Ok(safe_head_at_l1_block)
        } else {
            // Estimate the L1 block necessary based on the chain config. This is based on the maximum
            // delay between batches being posted on the L2 chain.
            let max_batch_post_delay_minutes = match self.rollup_config.l2_chain_id {
                11155420 => 10,
                10 => 10,
                8453 => 10,
                _ => 60,
            };

            // Get L1 head.
            let l2_block_timestamp = self
                .get_header_by_number(RPCMode::L2, l2_end_block)
                .await?
                .timestamp;

            let target_timestamp = l2_block_timestamp + (max_batch_post_delay_minutes * 60);
            Ok(self
                .find_block_hash_by_timestamp(RPCMode::L1, target_timestamp)
                .await?)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::fetcher::{OPSuccinctDataFetcher, RPCMode};

    #[tokio::test]
    #[cfg(test)]
    async fn test_get_l1_head() {
        dotenv::dotenv().ok();
        let fetcher = OPSuccinctDataFetcher::new().await;
        let latest_l2_block = fetcher.get_head(RPCMode::L2).await.unwrap();

        // Get the L2 block number from 1 hour ago.
        let l2_end_block = latest_l2_block.number - ((60 * 60) / fetcher.rollup_config.block_time);

        let _ = fetcher.get_l1_head(l2_end_block).await.unwrap();
    }
}
