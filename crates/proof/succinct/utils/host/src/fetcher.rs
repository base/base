use std::{
    cmp::{Ordering, min},
    env, fmt, fs,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_consensus::{BlockHeader, Header};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_network::{BlockResponse, Network, primitives::HeaderResponse};
use alloy_primitives::{Address, B256, Bytes, U64, U256, keccak256};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rlp::Decodable;
use alloy_sol_types::SolValue;
use anyhow::{Context, Result, anyhow, bail};
use base_common_consensus::BaseBlock;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_proof_host::HostConfig;
use base_proof_succinct_client_utils::{
    boot::BootInfoStruct, client::DEFAULT_INTERMEDIATE_ROOT_INTERVAL,
};
use base_protocol::L2BlockInfo;
use futures::{StreamExt, stream};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    L2Output,
    rpc_types::{OutputResponse, SafeHeadResponse},
};

#[derive(Clone)]
/// The `OPSuccinctDataFetcher` struct is used to fetch the L2 output data and L2 claim data for a
/// given block number. It is used to generate the boot info for the native host program.
/// FIXME: Add retries for all requests (3 retries).
pub struct OPSuccinctDataFetcher {
    /// RPC endpoint configuration.
    pub rpc_config: RPCConfig,
    /// L1 RPC provider.
    pub l1_provider: Arc<RootProvider>,
    /// L2 RPC provider.
    pub l2_provider: Arc<RootProvider<Base>>,
    /// Optional rollup config override.
    pub rollup_config: Option<RollupConfig>,
    /// Path to rollup config file.
    pub rollup_config_path: Option<PathBuf>,
    /// Path to L1 chain config file.
    pub l1_config_path: Option<PathBuf>,
}

impl fmt::Debug for OPSuccinctDataFetcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OPSuccinctDataFetcher").finish_non_exhaustive()
    }
}

impl Default for OPSuccinctDataFetcher {
    fn default() -> Self {
        Self::new()
    }
}

/// RPC endpoint URLs for L1 and L2.
#[derive(Debug, Clone)]
pub struct RPCConfig {
    /// L1 execution RPC URL.
    pub l1_rpc: Url,
    /// L1 beacon RPC URL (optional).
    pub l1_beacon_rpc: Option<Url>,
    /// L2 execution RPC URL.
    pub l2_rpc: Url,
    // TODO(fakedev9999): Make optional if possible.
    /// L2 consensus node RPC URL.
    pub l2_node_rpc: Url,
}

/// The mode corresponding to the chain we are fetching data for.
#[derive(Clone, Copy, Debug)]
pub enum RPCMode {
    /// L1 execution layer.
    L1,
    /// L1 beacon chain.
    L1Beacon,
    /// L2 execution layer.
    L2,
    /// L2 consensus node.
    L2Node,
}

/// Gets the RPC URLs from environment variables.
///
/// `L1_RPC`: The L1 RPC URL.
/// `L1_BEACON_RPC`: The L1 beacon RPC URL.
/// `L2_RPC`: The L2 RPC URL.
/// `L2_NODE_RPC`: The L2 node RPC URL.
pub fn get_rpcs_from_env() -> RPCConfig {
    let l1_rpc = env::var("L1_RPC").expect("L1_RPC must be set");
    let maybe_l1_beacon_rpc = env::var("L1_BEACON_RPC").ok();

    // L1_BEACON_RPC is optional. If not set or empty, set to None.
    let l1_beacon_rpc = maybe_l1_beacon_rpc
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Url::parse(s).expect("L1_BEACON_RPC must be a valid URL"));

    let l2_rpc = env::var("L2_RPC").expect("L2_RPC must be set");
    let l2_node_rpc = env::var("L2_NODE_RPC").expect("L2_NODE_RPC must be set");

    RPCConfig {
        l1_rpc: Url::parse(&l1_rpc).expect("L1_RPC must be a valid URL"),
        l1_beacon_rpc,
        l2_rpc: Url::parse(&l2_rpc).expect("L2_RPC must be a valid URL"),
        l2_node_rpc: Url::parse(&l2_node_rpc).expect("L2_NODE_RPC must be a valid URL"),
    }
}

/// The info to fetch for a block.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlockInfo {
    /// Block number.
    pub block_number: u64,
    /// Transaction count.
    pub transaction_count: u64,
    /// Gas used.
    pub gas_used: u64,
    /// Total L1 fees.
    pub total_l1_fees: u128,
    /// Total transaction fees.
    pub total_tx_fees: u128,
}

/// The fee data for a block.
#[derive(Debug)]
pub struct FeeData {
    /// Block number.
    pub block_number: u64,
    /// Transaction index.
    pub tx_index: u64,
    /// Transaction hash.
    pub tx_hash: B256,
    /// L1 gas cost.
    pub l1_gas_cost: U256,
    /// Transaction fee.
    pub tx_fee: u128,
}

impl OPSuccinctDataFetcher {
    /// Gets the RPC URL's and saves the rollup config for the chain to the rollup config file.
    pub fn new() -> Self {
        let rpc_config = get_rpcs_from_env();

        let l1_provider =
            Arc::new(ProviderBuilder::default().connect_http(rpc_config.l1_rpc.clone()));
        let l2_provider =
            Arc::new(ProviderBuilder::default().connect_http(rpc_config.l2_rpc.clone()));

        Self {
            rpc_config,
            l1_provider,
            l2_provider,
            rollup_config: None,
            rollup_config_path: None,
            l1_config_path: None,
        }
    }

    /// Initialize the fetcher with a rollup config, reading RPC URLs from environment variables.
    pub async fn new_with_rollup_config() -> Result<Self> {
        let rpc_config = get_rpcs_from_env();
        Self::from_rpc_config_with_rollup_config(rpc_config).await
    }

    /// Initialize the fetcher with an explicit [`RPCConfig`] and a rollup config.
    ///
    /// Prefer this over [`new_with_rollup_config`](Self::new_with_rollup_config) when the RPC
    /// URLs are already known, avoiding reliance on environment variables.
    pub async fn from_rpc_config_with_rollup_config(rpc_config: RPCConfig) -> Result<Self> {
        let l1_provider =
            Arc::new(ProviderBuilder::default().connect_http(rpc_config.l1_rpc.clone()));
        let l2_provider =
            Arc::new(ProviderBuilder::default().connect_http(rpc_config.l2_rpc.clone()));

        let (rollup_config, rollup_config_path) =
            Self::fetch_and_save_rollup_config(&rpc_config).await?;

        // Add warning if the chain is pre-Holocene, as derivation is significantly slower.
        let unix_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        if !rollup_config.is_holocene_active(unix_timestamp) {
            tracing::warn!(
                "Chain is not using Holocene hard fork. This will cause significant performance degradation compared to chains that have activated Holocene."
            );
        }

        // Fetch and save L1 config based on the rollup config's L1 chain ID
        let l1_config_path = Self::fetch_and_save_l1_config(&rollup_config).await?;

        Ok(Self {
            rpc_config,
            l1_provider,
            l2_provider,
            rollup_config: Some(rollup_config),
            rollup_config_path: Some(rollup_config_path),
            l1_config_path: Some(l1_config_path),
        })
    }

    /// Fetch the L2 chain ID.
    pub async fn get_l2_chain_id(&self) -> Result<u64> {
        Ok(self.l2_provider.get_chain_id().await?)
    }

    /// Fetch the latest L2 block header.
    pub async fn get_l2_head(&self) -> Result<Header> {
        let block = self.l2_provider.get_block_by_number(BlockNumberOrTag::Latest).await?;
        if let Some(block) = block {
            Ok(block.header.inner)
        } else {
            bail!("Failed to get L2 head");
        }
    }

    /// Get the aggregate block statistics for a range of blocks exclusive of the start block.
    ///
    /// When proving a range in OP Succinct, we are proving the transition from the block hash
    /// of the start block to the block hash of the end block. This means that we don't expend
    /// resources to "prove" the start block. This is why the start block is not included in the
    /// range for which we fetch block data.
    pub async fn get_l2_block_data_range(&self, start: u64, end: u64) -> Result<Vec<BlockInfo>> {
        use futures::stream::{self, StreamExt};

        let block_data = stream::iter(start + 1..=end)
            .map(|block_number| async move {
                let block =
                    self.l2_provider.get_block_by_number(block_number.into()).await?.unwrap();
                let receipts =
                    self.l2_provider.get_block_receipts(block_number.into()).await?.unwrap();
                let total_l1_fees: u128 =
                    receipts.iter().map(|tx| tx.l1_block_info.l1_fee.unwrap_or(0)).sum();
                let total_tx_fees: u128 = receipts
                    .iter()
                    .map(|tx| {
                        // tx.inner.effective_gas_price * tx.inner.gas_used +
                        // tx.l1_block_info.l1_fee is the total fee for the transaction.
                        // tx.inner.effective_gas_price * tx.inner.gas_used is the tx fee on L2.
                        tx.inner.effective_gas_price * tx.inner.gas_used as u128
                            + tx.l1_block_info.l1_fee.unwrap_or(0)
                    })
                    .sum();

                Ok(BlockInfo {
                    block_number,
                    transaction_count: block.transactions.len() as u64,
                    gas_used: block.header.gas_used,
                    total_l1_fees,
                    total_tx_fees,
                })
            })
            .buffered(10)
            .collect::<Vec<Result<BlockInfo>>>()
            .await;

        block_data.into_iter().collect()
    }

    /// Fetch an L1 block header by ID.
    pub async fn get_l1_header(&self, block_number: BlockId) -> Result<Header> {
        let block = self.l1_provider.get_block(block_number).await?;

        if let Some(block) = block {
            Ok(block.header.inner)
        } else {
            bail!("Failed to get L1 header for block {block_number}");
        }
    }

    /// Fetch an L2 block header by ID.
    pub async fn get_l2_header(&self, block_number: BlockId) -> Result<Header> {
        let block = self.l2_provider.get_block(block_number).await?;

        if let Some(block) = block {
            Ok(block.header.inner)
        } else {
            bail!("Failed to get L1 header for block {block_number}");
        }
    }

    /// Finds the L1 block at the provided timestamp.
    pub async fn find_l1_block_by_timestamp(&self, target_timestamp: u64) -> Result<(B256, u64)> {
        self.find_block_by_timestamp(&self.l1_provider, target_timestamp).await
    }

    /// Finds the L2 block at the provided timestamp.
    pub async fn find_l2_block_by_timestamp(&self, target_timestamp: u64) -> Result<(B256, u64)> {
        self.find_block_by_timestamp(&self.l2_provider, target_timestamp).await
    }

    /// Finds the block at the provided timestamp, using the provided provider.
    async fn find_block_by_timestamp<N>(
        &self,
        provider: &RootProvider<N>,
        target_timestamp: u64,
    ) -> Result<(B256, u64)>
    where
        N: Network,
    {
        let latest_block = provider.get_block(BlockId::finalized()).await?;
        let mut low = 0;
        let mut high = if let Some(block) = latest_block {
            block.header().number()
        } else {
            bail!("Failed to get latest block");
        };

        while low <= high {
            let mid = (low + high) / 2;
            let block = provider.get_block(mid.into()).await?;
            if let Some(block) = block {
                let block_timestamp = block.header().timestamp();

                match block_timestamp.cmp(&target_timestamp) {
                    Ordering::Equal => {
                        return Ok((block.header().hash().0.into(), block.header().number()));
                    }
                    Ordering::Less => low = mid + 1,
                    Ordering::Greater => high = mid - 1,
                }
            } else {
                bail!("Failed to get block for block {mid}");
            }
        }

        // Return the block hash of the closest block after the target timestamp
        let block = provider.get_block(low.into()).await?;
        if let Some(block) = block {
            Ok((block.header().hash().0.into(), block.header().number()))
        } else {
            bail!("Failed to get block for block {low}");
        }
    }

    /// Get the RPC URL for the given RPC mode.
    pub fn get_rpc_url(&self, rpc_mode: RPCMode) -> Result<&Url> {
        match rpc_mode {
            RPCMode::L1 => Ok(&self.rpc_config.l1_rpc),
            RPCMode::L2 => Ok(&self.rpc_config.l2_rpc),
            RPCMode::L1Beacon => self
                .rpc_config
                .l1_beacon_rpc
                .as_ref()
                .ok_or_else(|| anyhow!("L1 beacon RPC URL is not set")),
            RPCMode::L2Node => Ok(&self.rpc_config.l2_node_rpc),
        }
    }

    /// Fetch and save the rollup config to a temporary file.
    async fn fetch_and_save_rollup_config(
        rpc_config: &RPCConfig,
    ) -> Result<(RollupConfig, PathBuf)> {
        let rollup_config: RollupConfig =
            Self::fetch_rpc_data(&rpc_config.l2_node_rpc, "optimism_rollupConfig", vec![]).await?;

        // Create configs directory if it doesn't exist
        let default_dir = PathBuf::from("configs/L2");
        let l2_config_dir = env::var("L2_CONFIG_DIR").map(PathBuf::from).unwrap_or(default_dir);
        fs::create_dir_all(&l2_config_dir)?;

        // Save rollup config to a file named by chain ID
        let rollup_config_path = l2_config_dir.join(format!("{}.json", rollup_config.l2_chain_id));

        // Write the rollup config to the file
        let rollup_config_str = serde_json::to_string_pretty(&rollup_config)?;
        fs::write(&rollup_config_path, rollup_config_str)?;

        tracing::info!(
            "Saved L2 config for chain ID {} to {}",
            rollup_config.l2_chain_id,
            rollup_config_path.display()
        );

        // Return both the rollup config and the path to the temporary file
        Ok((rollup_config, rollup_config_path))
    }

    /// Fetch and save the L1 config based on the rollup config's L1 chain ID.
    async fn fetch_and_save_l1_config(rollup_config: &RollupConfig) -> Result<PathBuf> {
        let default_dir = PathBuf::from("configs/L1");
        let l1_config_dir = env::var("L1_CONFIG_DIR").map(PathBuf::from).unwrap_or(default_dir);

        // Check if the L1 config file exists. If it does, return the path to the file.
        let l1_config_path = l1_config_dir.join(format!("{}.json", rollup_config.l1_chain_id));
        if l1_config_path.exists() {
            tracing::info!(
                chain_id = rollup_config.l1_chain_id,
                path = %l1_config_path.display(),
                "l1 config already exists"
            );

            let file = fs::File::open(&l1_config_path)?;
            let l1_config: Value = serde_json::from_reader(file)?;
            tracing::debug!(
                chain_id = rollup_config.l1_chain_id,
                config = ?l1_config,
                "loaded l1 config from file"
            );

            return Ok(l1_config_path);
        }

        // Lookup the L1 config from the registry.
        let l1_config =
            base_common_chains::L1_CONFIGS.get(&rollup_config.l1_chain_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "No built-in L1 config exists for chain ID {}.\n\
                 To proceed, either:\n\
                 • Create a config file at: {}\n\
                 • Or set L1_CONFIG_DIR to the directory containing <chain_id>.json",
                    rollup_config.l1_chain_id,
                    l1_config_path.display()
                )
            })?;

        tracing::debug!(
            chain_id = rollup_config.l1_chain_id,
            config = ?l1_config,
            "fetched l1 config from built-in mapping"
        );

        // Create the L1 config directory if it doesn't exist.
        fs::create_dir_all(&l1_config_dir)
            .with_context(|| format!("creating {}", l1_config_dir.display()))?;

        // Write the L1 config to the file
        let l1_config_str = serde_json::to_string_pretty(l1_config)?;
        fs::write(&l1_config_path, l1_config_str)?;

        tracing::info!(
            chain_id = rollup_config.l1_chain_id,
            path = %l1_config_path.display(),
            "saved l1 config"
        );

        Ok(l1_config_path)
    }

    async fn fetch_rpc_data<T>(url: &Url, method: &str, params: Vec<Value>) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let client = reqwest::Client::new();
        let response = client
            .post(url.clone())
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
            return Err(anyhow::anyhow!("Error calling {method}: {error_message}"));
        }

        serde_json::from_value(response["result"].clone()).map_err(Into::into)
    }

    /// Fetch arbitrary data from the RPC.
    pub async fn fetch_rpc_data_with_mode<T>(
        &self,
        rpc_mode: RPCMode,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let url = self.get_rpc_url(rpc_mode)?;
        Self::fetch_rpc_data(url, method, params).await
    }

    /// Get the earliest L1 header in a batch of boot infos.
    pub async fn get_earliest_l1_head_in_batch(
        &self,
        boot_infos: &Vec<BootInfoStruct>,
    ) -> Result<Header> {
        let mut earliest_block_num: u64 = u64::MAX;
        let mut earliest_l1_header: Option<Header> = None;

        for boot_info in boot_infos {
            let l1_block_header = self.get_l1_header(boot_info.l1Head.into()).await?;
            if l1_block_header.number < earliest_block_num {
                earliest_block_num = l1_block_header.number;
                earliest_l1_header = Some(l1_block_header);
            }
        }
        Ok(earliest_l1_header.unwrap())
    }

    /// Get the latest L1 header in a batch of boot infos.
    pub async fn get_latest_l1_head_in_batch(
        &self,
        boot_infos: &Vec<BootInfoStruct>,
    ) -> Result<Header> {
        let mut latest_block_num: u64 = u64::MIN;
        let mut latest_l1_header: Option<Header> = None;

        for boot_info in boot_infos {
            let l1_block_header = self.get_l1_header(boot_info.l1Head.into()).await?;
            if l1_block_header.number > latest_block_num {
                latest_block_num = l1_block_header.number;
                latest_l1_header = Some(l1_block_header);
            }
        }
        if let Some(header) = latest_l1_header {
            Ok(header)
        } else {
            bail!("Failed to get latest L1 header");
        }
    }

    /// Fetch headers for a range of blocks inclusive.
    pub async fn fetch_headers_in_range(&self, start: u64, end: u64) -> Result<Vec<Header>> {
        let block_numbers: Vec<u64> = (start..=end).collect();
        let mut headers = Vec::new();

        // Process blocks in batches of 10, but maintain original order
        let results = stream::iter(block_numbers)
            .map(|block_number| self.get_l1_header(block_number.into()))
            .buffered(10)
            .collect::<Vec<_>>()
            .await;

        for result in results {
            headers.push(result?);
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
        let latest_header = self.get_l1_header(checkpoint_block_hash.into()).await?;

        // Create a vector of futures for fetching all headers
        let headers =
            self.fetch_headers_in_range(start_header.number, latest_header.number).await?;

        Ok(headers)
    }

    /// Fetch the L2 output at a given block number.
    pub async fn get_l2_output_at_block(&self, block_number: u64) -> Result<OutputResponse> {
        let block_number_hex = format!("0x{block_number:x}");
        let l2_output_data: OutputResponse = self
            .fetch_rpc_data_with_mode(
                RPCMode::L2Node,
                "optimism_outputAtBlock",
                vec![block_number_hex.into()],
            )
            .await?;
        Ok(l2_output_data)
    }

    /// Get the L1 block from which the `l2_end_block` can be derived.
    ///
    /// Use binary search to find the first L1 block with an L2 safe head >= `l2_end_block`.
    pub async fn get_safe_l1_block_for_l2_block(&self, l2_end_block: u64) -> Result<(B256, u64)> {
        let latest_l1_header = self.get_l1_header(BlockId::finalized()).await?;

        // Get the l1 origin of the l2 end block.
        let l2_end_block_hex = format!("0x{l2_end_block:x}");
        let optimism_output_data: OutputResponse = self
            .fetch_rpc_data_with_mode(
                RPCMode::L2Node,
                "optimism_outputAtBlock",
                vec![l2_end_block_hex.into()],
            )
            .await?;

        let l1_origin = optimism_output_data.block_ref.l1_origin;

        // Binary search for the first L1 block with L2 safe head >= l2_end_block.
        let mut low = l1_origin.number;
        let mut high = latest_l1_header.number;
        let mut first_valid = None;

        while low <= high {
            let mid = low + (high - low) / 2;
            let l1_block_number_hex = format!("0x{mid:x}");
            let result: SafeHeadResponse = self
                .fetch_rpc_data_with_mode(
                    RPCMode::L2Node,
                    "optimism_safeHeadAtL1Block",
                    vec![l1_block_number_hex.into()],
                )
                .await?;
            let l2_safe_head = result.safe_head.number;

            if l2_safe_head >= l2_end_block {
                // Found a valid block, save it and keep searching lower.
                first_valid = Some((result.l1_block.hash, result.l1_block.number));
                high = mid - 1;
            } else {
                // Need to search higher
                low = mid + 1;
            }
        }

        first_valid.ok_or_else(|| {
            anyhow::anyhow!(
                "Could not find an L1 block with an L2 safe head greater than the L2 end block."
            )
        })
    }

    /// If the safeDB is activated, use it to fetch the L1 block where the batch including the data
    /// for the end L2 block was posted. If the safeDB is not activated:
    ///   - If `safe_db_fallback` is `true`, estimate the L1 head based on the L2 block timestamp.
    ///   - Else, return an error.
    pub async fn get_l1_head(
        &self,
        l2_end_block: u64,
        safe_db_fallback: bool,
    ) -> Result<(B256, u64)> {
        if self.rollup_config.is_none() {
            return Err(anyhow::anyhow!("Rollup config not loaded."));
        }

        match self.get_safe_l1_block_for_l2_block(l2_end_block).await {
            Ok(safe_head) => Ok(safe_head),
            Err(e) => {
                if safe_db_fallback {
                    tracing::warn!(
                        "SafeDB not activated - falling back to timestamp-based L1 head estimation. WARNING: This fallback method is more expensive and less reliable. Derivation may fail if the L2 block batch is posted after our estimated L1 head. Enable SafeDB on op-node to fix this."
                    );
                    // Fallback: estimate L1 block based on timestamp
                    let max_batch_post_delay_minutes = 40;
                    let l2_block_timestamp =
                        self.get_l2_header(l2_end_block.into()).await?.timestamp;
                    let finalized_l1_timestamp =
                        self.get_l1_header(BlockId::finalized()).await?.timestamp;

                    let target_timestamp = min(
                        l2_block_timestamp + (max_batch_post_delay_minutes * 60),
                        finalized_l1_timestamp,
                    );
                    self.find_l1_block_by_timestamp(target_timestamp).await
                } else {
                    Err(anyhow::anyhow!(
                        "SafeDB is not activated on your op-node and the `SAFE_DB_FALLBACK` flag is set to false. Please enable the safeDB on your op-node to fix this, or set `SAFE_DB_FALLBACK` flag to true, which will be more expensive: {e}"
                    ))
                }
            }
        }
    }

    // Source from: https://github.com/anton-rs/kona/blob/85b1c88b44e5f54edfc92c781a313717bad5dfc7/crates/derive-alloy/src/alloy_providers.rs#L225.
    /// Fetch an L2 block by number.
    pub async fn get_l2_block_by_number(&self, block_number: u64) -> Result<BaseBlock> {
        let raw_block: Bytes = self
            .l2_provider
            .raw_request("debug_getRawBlock".into(), [U64::from(block_number)])
            .await?;
        let block = BaseBlock::decode(&mut raw_block.as_ref()).map_err(|e| anyhow::anyhow!(e))?;
        Ok(block)
    }

    /// Fetch L2 block info by number.
    pub async fn l2_block_info_by_number(&self, block_number: u64) -> Result<L2BlockInfo> {
        // If the rollup config is not already loaded, fetch and save it.
        if self.rollup_config.is_none() {
            return Err(anyhow::anyhow!("Rollup config not loaded."));
        }
        let genesis = self.rollup_config.as_ref().unwrap().genesis;
        let block = self.get_l2_block_by_number(block_number).await?;
        Ok(L2BlockInfo::from_block_and_genesis(&block, &genesis)?)
    }

    /// Get the L2 safe head corresponding to the L1 block number using `optimism_safeHeadAtL1Block`.
    pub async fn get_l2_safe_head_from_l1_block_number(&self, l1_block_number: u64) -> Result<u64> {
        let l1_block_number_hex = format!("0x{l1_block_number:x}");
        let result: SafeHeadResponse = self
            .fetch_rpc_data_with_mode(
                RPCMode::L2Node,
                "optimism_safeHeadAtL1Block",
                vec![l1_block_number_hex.into()],
            )
            .await?;
        Ok(result.safe_head.number)
    }

    /// Check if the safeDB is activated on the L2 node.
    pub async fn is_safe_db_activated(&self) -> Result<bool> {
        let finalized_l1_header = self.get_l1_header(BlockId::finalized()).await?;
        let l1_block_number_hex = format!("0x{:x}", finalized_l1_header.number);
        let result: Result<SafeHeadResponse, _> = self
            .fetch_rpc_data_with_mode(
                RPCMode::L2Node,
                "optimism_safeHeadAtL1Block",
                vec![l1_block_number_hex.into()],
            )
            .await;
        Ok(result.is_ok())
    }

    /// Get the L2 output data for a given block number and save the boot info to a file in the data
    /// directory with `block_number`. Return the arguments to be passed to the native host for
    /// datagen.
    pub async fn get_host_args(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        l1_head_hash: B256,
    ) -> Result<HostConfig> {
        let Some(rollup_config) = &self.rollup_config else {
            return Err(anyhow::anyhow!("Rollup config not loaded."));
        };

        if l2_start_block >= l2_end_block {
            return Err(anyhow::anyhow!(
                "L2 start block is greater than or equal to L2 end block. Start: {l2_start_block}, End: {l2_end_block}"
            ));
        }

        let l2_provider = Arc::clone(&self.l2_provider);

        // Get L2 output data.
        let l2_output_block = l2_provider
            .get_block_by_number(l2_start_block.into())
            .await?
            .ok_or_else(|| anyhow::anyhow!("Block not found for block number {l2_start_block}"))?;
        let l2_output_state_root = l2_output_block.header.state_root;
        let agreed_l2_head_hash = l2_output_block.header.hash;
        let l2_output_storage_hash = l2_provider
            .get_proof(Address::from_str("0x4200000000000000000000000000000000000016")?, Vec::new())
            .block_id(l2_start_block.into())
            .await?
            .storage_hash;

        let l2_output_encoded = L2Output {
            zero: 0,
            l2_state_root: l2_output_state_root.0.into(),
            l2_storage_hash: l2_output_storage_hash.0.into(),
            l2_claim_hash: agreed_l2_head_hash.0.into(),
        };
        let agreed_l2_output_root = keccak256(l2_output_encoded.abi_encode());

        // Get L2 claim data.
        let l2_claim_block = l2_provider.get_block_by_number(l2_end_block.into()).await?.unwrap();
        let l2_claim_state_root = l2_claim_block.header.state_root;
        let l2_claim_hash = l2_claim_block.header.hash;
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
        let claimed_l2_output_root = keccak256(l2_claim_encoded.abi_encode());

        let l1_beacon_url = self
            .rpc_config
            .l1_beacon_rpc
            .as_ref()
            .map(|addr| addr.as_str().trim_end_matches('/').to_string())
            .unwrap_or_default();

        // Load L1 config from file or registry.
        let l1_config = if let Some(ref l1_config_path) = self.l1_config_path {
            let file = fs::File::open(l1_config_path)?;
            serde_json::from_reader(file)?
        } else {
            base_common_chains::L1_CONFIGS
                .get(&rollup_config.l1_chain_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("No L1 config for chain ID {}", rollup_config.l1_chain_id)
                })?
                .clone()
        };

        let l1_head_number = self.get_l1_header(l1_head_hash.into()).await?.number;

        let request = base_proof_primitives::ProofRequest {
            l1_head: l1_head_hash,
            agreed_l2_output_root,
            agreed_l2_head_hash,
            claimed_l2_output_root,
            claimed_l2_block_number: l2_end_block,
            intermediate_block_interval: DEFAULT_INTERMEDIATE_ROOT_INTERVAL,
            l1_head_number,
            // We don't need to set the proposer or image hash for the range proof zk program
            proposer: Address::ZERO,
            image_hash: B256::ZERO,
        };

        let prover = base_proof_host::ProverConfig {
            l1_eth_url: self.rpc_config.l1_rpc.as_str().trim_end_matches('/').to_string(),
            l2_eth_url: self.rpc_config.l2_rpc.as_str().trim_end_matches('/').to_string(),
            l1_beacon_url,
            l2_chain_id: rollup_config.l2_chain_id.id(),
            rollup_config: rollup_config.clone(),
            l1_config,
            enable_experimental_witness_endpoint: true,
        };

        Ok(HostConfig { request, prover, data_dir: None })
    }
}
