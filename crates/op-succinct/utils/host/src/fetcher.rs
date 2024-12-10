use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{Address, B256},
    providers::{Provider, ProviderBuilder, RootProvider},
    transports::{
        http::{reqwest::Url, Client, Http},
        Transport,
    },
};
use alloy_consensus::{BlockHeader, Header};
use alloy_rlp::Decodable;
use alloy_sol_types::SolValue;
use anyhow::Result;
use anyhow::{anyhow, bail};
use cargo_metadata::MetadataCommand;
use futures::{stream, StreamExt};
use kona_host::HostCli;
use op_alloy_consensus::OpBlock;
use op_alloy_genesis::RollupConfig;
use op_alloy_network::{
    primitives::{BlockTransactions, BlockTransactionsKind, HeaderResponse},
    BlockResponse, Network, Optimism,
};
use op_alloy_protocol::calculate_tx_l1_cost_fjord;
use op_alloy_protocol::L2BlockInfo;
use op_alloy_rpc_types::{OpTransactionReceipt, OutputResponse, SafeHeadResponse};
use op_succinct_client_utils::boot::BootInfoStruct;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    cmp::{min, Ordering},
    collections::HashMap,
    env, fs,
    path::Path,
    str::FromStr,
    sync::Arc,
};

use alloy_primitives::{keccak256, Bytes, U256, U64};

use crate::{
    rollup_config::{get_rollup_config_path, merge_rollup_config},
    L2Output, ProgramType,
};

#[derive(Clone)]
/// The OPSuccinctDataFetcher struct is used to fetch the L2 output data and L2 claim data for a
/// given block number. It is used to generate the boot info for the native host program.
/// FIXME: Add retries for all requests (3 retries).
pub struct OPSuccinctDataFetcher {
    pub rpc_config: RPCConfig,
    pub l1_provider: Arc<RootProvider<Http<Client>>>,
    pub l2_provider: Arc<RootProvider<Http<Client>, Optimism>>,
    pub rollup_config: Option<RollupConfig>,
    pub run_context: RunContext,
}

impl Default for OPSuccinctDataFetcher {
    fn default() -> Self {
        OPSuccinctDataFetcher::new(RunContext::Dev)
    }
}

#[derive(Debug, Clone)]
pub struct RPCConfig {
    pub l1_rpc: Url,
    pub l1_beacon_rpc: Url,
    pub l2_rpc: Url,
    pub l2_node_rpc: Url,
}

/// The mode corresponding to the chain we are fetching data for.
#[derive(Clone, Copy, Debug)]
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

/// Dev or Docker context.
#[derive(Clone, Copy)]
pub enum RunContext {
    Dev,
    Docker,
}

fn get_rpcs() -> RPCConfig {
    let l1_rpc = env::var("L1_RPC").expect("L1_RPC must be set");
    let l1_beacon_rpc = env::var("L1_BEACON_RPC").expect("L1_BEACON_RPC must be set");
    let l2_rpc = env::var("L2_RPC").expect("L2_RPC must be set");
    let l2_node_rpc = env::var("L2_NODE_RPC").expect("L2_NODE_RPC must be set");

    RPCConfig {
        l1_rpc: Url::parse(&l1_rpc).expect("L1_RPC must be a valid URL"),
        l1_beacon_rpc: Url::parse(&l1_beacon_rpc).expect("L1_BEACON_RPC must be a valid URL"),
        l2_rpc: Url::parse(&l2_rpc).expect("L2_RPC must be a valid URL"),
        l2_node_rpc: Url::parse(&l2_node_rpc).expect("L2_NODE_RPC must be a valid URL"),
    }
}

/// The info to fetch for a block.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlockInfo {
    pub block_number: u64,
    pub transaction_count: u64,
    pub gas_used: u64,
    pub total_l1_fees: u128,
    pub total_tx_fees: u128,
}

/// The fee data for a block.
pub struct FeeData {
    pub block_number: u64,
    pub tx_index: u64,
    pub tx_hash: B256,
    pub l1_gas_cost: U256,
    pub tx_fee: u128,
}

impl OPSuccinctDataFetcher {
    /// Gets the RPC URL's and saves the rollup config for the chain to the rollup config file.
    pub fn new(run_context: RunContext) -> Self {
        let rpc_config = get_rpcs();

        let l1_provider = Arc::new(ProviderBuilder::default().on_http(rpc_config.l1_rpc.clone()));
        let l2_provider = Arc::new(ProviderBuilder::default().on_http(rpc_config.l2_rpc.clone()));

        OPSuccinctDataFetcher {
            rpc_config,
            l1_provider,
            l2_provider,
            rollup_config: None,
            run_context,
        }
    }

    /// Initialize the fetcher with a rollup config.
    pub async fn new_with_rollup_config(run_context: RunContext) -> Result<Self> {
        let rpc_config = get_rpcs();

        let l1_provider = Arc::new(ProviderBuilder::default().on_http(rpc_config.l1_rpc.clone()));
        let l2_provider = Arc::new(ProviderBuilder::default().on_http(rpc_config.l2_rpc.clone()));

        let rollup_config = Self::fetch_and_save_rollup_config(&rpc_config, run_context).await?;

        Ok(OPSuccinctDataFetcher {
            rpc_config,
            l1_provider,
            l2_provider,
            rollup_config: Some(rollup_config),
            run_context,
        })
    }

    pub async fn get_l2_chain_id(&self) -> Result<u64> {
        Ok(self.l2_provider.get_chain_id().await?)
    }

    pub async fn get_l2_head(&self) -> Result<Header> {
        let block = self
            .l2_provider
            .get_block_by_number(BlockNumberOrTag::Latest, BlockTransactionsKind::Hashes)
            .await?;
        if let Some(block) = block {
            Ok(block.header.inner)
        } else {
            bail!("Failed to get L2 head");
        }
    }

    pub async fn get_l2_header_by_number(&self, block_number: u64) -> Result<Header> {
        let block = self
            .l2_provider
            .get_block_by_number(block_number.into(), BlockTransactionsKind::Hashes)
            .await?;
        if let Some(block) = block {
            Ok(block.header.inner)
        } else {
            bail!("Failed to get L2 header for block {block_number}");
        }
    }

    /// Manually calculate the L1 fee data for a range of blocks. Allows for modifying the L1 fee scalar.
    pub async fn get_l2_fee_data_with_modified_l1_fee_scalar(
        &self,
        start: u64,
        end: u64,
        custom_l1_fee_scalar: Option<U256>,
    ) -> Result<Vec<FeeData>> {
        use futures::stream::{self, StreamExt};

        // Fetch all tranasctions in parallel.
        // Return a tuple of the block number and the transactions.
        let transactions: Vec<(u64, Vec<B256>)> = stream::iter(start..=end)
            .map(|block_number| async move {
                let block = self
                    .l2_provider
                    .get_block(block_number.into(), BlockTransactionsKind::Hashes)
                    .await?;
                if let Some(block) = block {
                    match block.transactions {
                        BlockTransactions::Hashes(txs) => Ok((block_number, txs)),
                        _ => Err(anyhow::anyhow!("Unsupported transaction type")),
                    }
                } else {
                    bail!("Failed to get L2 block for block {block_number}");
                }
            })
            .buffered(100)
            .collect::<Vec<Result<(u64, Vec<B256>)>>>()
            .await
            .into_iter()
            .filter_map(Result::ok)
            .collect();

        // Create a map of the block number to the transactions.
        let block_number_to_transactions: HashMap<u64, Vec<B256>> =
            transactions.into_iter().collect();

        // Fetch all of the L1 block receipts in parallel.
        let block_receipts: Vec<(u64, Vec<OpTransactionReceipt>)> = stream::iter(start..=end)
            .map(|block_number| async move {
                let receipts = self
                    .l2_provider
                    .get_block_receipts(block_number.into())
                    .await?;
                if let Some(receipts) = receipts {
                    Ok((block_number, receipts))
                } else {
                    bail!("Failed to get L2 receipts for block {block_number}");
                }
            })
            .buffered(100)
            .collect::<Vec<Result<(u64, Vec<OpTransactionReceipt>), anyhow::Error>>>()
            .await
            .into_iter()
            .filter_map(Result::ok)
            .collect();

        // Get all the encoded transactions for each block number in parallel.
        let block_number_to_encoded_transactions = stream::iter(block_number_to_transactions)
            .map(|(block_number, transactions)| async move {
                let encoded_transactions = stream::iter(transactions)
                    .map(|tx_hash| async move {
                        self.l2_provider
                            .client()
                            .request::<&[B256; 1], Bytes>("debug_getRawTransaction", &[tx_hash])
                            .await
                            .map_err(|e| anyhow!("Error fetching transaction: {e}"))
                            .unwrap()
                    })
                    .buffered(100)
                    .collect::<Vec<Bytes>>()
                    .await;
                (block_number, encoded_transactions)
            })
            .buffered(100)
            .collect::<HashMap<u64, Vec<Bytes>>>()
            .await;

        // Zip the block number to encoded transactions with the block number to receipts.
        let block_number_to_receipts_and_transactions: HashMap<
            u64,
            (Vec<OpTransactionReceipt>, Vec<Bytes>),
        > = block_receipts
            .into_iter()
            .filter_map(|(block_number, receipts)| {
                block_number_to_encoded_transactions
                    .get(&block_number)
                    .map(|transactions| (block_number, (receipts, transactions.clone())))
            })
            .collect();

        let mut fee_data = Vec::new();
        for (block_number, (receipts, transactions)) in block_number_to_receipts_and_transactions {
            for (transaction, receipt) in transactions.iter().zip(receipts) {
                let l1_fee_scalar = if let Some(custom_l1_fee_scalar) = custom_l1_fee_scalar {
                    custom_l1_fee_scalar
                } else {
                    U256::from(receipt.l1_block_info.l1_base_fee_scalar.unwrap_or(0))
                };
                // Get the Fjord L1 cost of the transaction.
                let l1_gas_cost = calculate_tx_l1_cost_fjord(
                    transaction.as_ref(),
                    U256::from(receipt.l1_block_info.l1_gas_price.unwrap_or(0)),
                    l1_fee_scalar,
                    U256::from(receipt.l1_block_info.l1_blob_base_fee.unwrap_or(0)),
                    U256::from(receipt.l1_block_info.l1_blob_base_fee_scalar.unwrap_or(0)),
                );

                fee_data.push(FeeData {
                    block_number,
                    tx_index: receipt.inner.transaction_index.unwrap(),
                    tx_hash: receipt.inner.transaction_hash,
                    l1_gas_cost,
                    tx_fee: receipt.inner.effective_gas_price * receipt.inner.gas_used,
                });
            }
        }
        Ok(fee_data)
    }

    /// Get the fee data for a range of blocks. Extracts the l1 fee data from the receipts.
    pub async fn get_l2_fee_data_range(&self, start: u64, end: u64) -> Result<Vec<FeeData>> {
        let l2_provider = self.l2_provider.clone();

        use futures::stream::{self, StreamExt};

        // Only fetch 100 receipts at a time to better use system resources. Increases stability.
        let fee_data = stream::iter(start..=end)
            .map(|block_number| {
                let l2_provider = l2_provider.clone();
                async move {
                    let receipt = l2_provider
                        .get_block_receipts(block_number.into())
                        .await
                        .unwrap();
                    let transactions = receipt.unwrap();
                    let block_fee_data: Vec<FeeData> = transactions
                        .iter()
                        .enumerate()
                        .map(|(tx_index, tx)| FeeData {
                            block_number,
                            tx_index: tx_index as u64,
                            tx_hash: tx.inner.transaction_hash,
                            l1_gas_cost: U256::from(tx.l1_block_info.l1_fee.unwrap_or(0)),
                            tx_fee: tx.inner.effective_gas_price * tx.inner.gas_used,
                        })
                        .collect();
                    block_fee_data
                }
            })
            .buffered(100)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();
        Ok(fee_data)
    }

    /// Get the aggregate block statistics for a range of blocks.
    pub async fn get_l2_block_data_range(&self, start: u64, end: u64) -> Result<Vec<BlockInfo>> {
        use futures::stream::{self, StreamExt};

        let block_data = stream::iter(start..=end)
            .map(|block_number| async move {
                let block = self
                    .l2_provider
                    .get_block_by_number(block_number.into(), BlockTransactionsKind::Hashes)
                    .await?
                    .unwrap();
                let receipts = self
                    .l2_provider
                    .get_block_receipts(block_number.into())
                    .await?
                    .unwrap();
                let total_l1_fees: u128 = receipts
                    .iter()
                    .map(|tx| tx.l1_block_info.l1_fee.unwrap_or(0))
                    .sum();
                let total_tx_fees: u128 = receipts
                    .iter()
                    .map(|tx| {
                        // tx.inner.effective_gas_price * tx.inner.gas_used + tx.l1_block_info.l1_fee is the total fee for the transaction.
                        // tx.inner.effective_gas_price * tx.inner.gas_used is the tx fee on L2.
                        tx.inner.effective_gas_price * tx.inner.gas_used
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
            .buffered(100)
            .collect::<Vec<Result<BlockInfo>>>()
            .await;

        block_data.into_iter().collect()
    }

    pub async fn get_l1_header(&self, block_number: BlockId) -> Result<Header> {
        let block = self
            .l1_provider
            .get_block(block_number, alloy::rpc::types::BlockTransactionsKind::Full)
            .await?;

        if let Some(block) = block {
            Ok(block.header.inner)
        } else {
            bail!("Failed to get L1 header for block {block_number}");
        }
    }

    pub async fn get_l2_header(&self, block_number: BlockId) -> Result<Header> {
        let block = self
            .l2_provider
            .get_block(block_number, alloy::rpc::types::BlockTransactionsKind::Full)
            .await?;

        if let Some(block) = block {
            Ok(block.header.inner)
        } else {
            bail!("Failed to get L1 header for block {block_number}");
        }
    }

    /// Finds the L1 block at the provided timestamp.
    pub async fn find_l1_block_by_timestamp(&self, target_timestamp: u64) -> Result<(B256, u64)> {
        self.find_block_by_timestamp(&self.l1_provider, target_timestamp)
            .await
    }

    /// Finds the L2 block at the provided timestamp.
    pub async fn find_l2_block_by_timestamp(&self, target_timestamp: u64) -> Result<(B256, u64)> {
        self.find_block_by_timestamp(&self.l2_provider, target_timestamp)
            .await
    }

    /// Finds the block at the provided timestamp, using the provided provider.
    async fn find_block_by_timestamp<P, T, N>(
        &self,
        provider: &P,
        target_timestamp: u64,
    ) -> Result<(B256, u64)>
    where
        P: Provider<T, N>,
        T: Transport + Clone,
        N: Network,
    {
        let latest_block = provider
            .get_block(BlockId::latest(), BlockTransactionsKind::Hashes)
            .await?;
        let mut low = 0;
        let mut high = if let Some(block) = latest_block {
            block.header().number()
        } else {
            bail!("Failed to get latest block");
        };

        while low <= high {
            let mid = (low + high) / 2;
            let block = provider
                .get_block(mid.into(), BlockTransactionsKind::Hashes)
                .await?;
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
        let block = provider
            .get_block(low.into(), BlockTransactionsKind::Hashes)
            .await?;
        if let Some(block) = block {
            Ok((block.header().hash().0.into(), block.header().number()))
        } else {
            bail!("Failed to get block for block {low}");
        }
    }

    /// Get the RPC URL for the given RPC mode.
    pub fn get_rpc_url(&self, rpc_mode: RPCMode) -> &Url {
        match rpc_mode {
            RPCMode::L1 => &self.rpc_config.l1_rpc,
            RPCMode::L2 => &self.rpc_config.l2_rpc,
            RPCMode::L1Beacon => &self.rpc_config.l1_beacon_rpc,
            RPCMode::L2Node => &self.rpc_config.l2_node_rpc,
        }
    }

    /// Fetch the rollup config. Combines the rollup config from `optimism_rollupConfig` and the
    /// chain config from `debug_chainConfig`. Saves the rollup config to the rollup config file and
    /// in memory.
    async fn fetch_and_save_rollup_config(
        rpc_config: &RPCConfig,
        run_context: RunContext,
    ) -> Result<RollupConfig> {
        let rollup_config =
            Self::fetch_rpc_data(&rpc_config.l2_node_rpc, "optimism_rollupConfig", vec![]).await?;
        let chain_config =
            Self::fetch_rpc_data(&rpc_config.l2_rpc, "debug_chainConfig", vec![]).await?;
        let rollup_config = merge_rollup_config(&rollup_config, &chain_config)?;

        // Save rollup config to the rollup config file.
        let rollup_config_path = get_rollup_config_path(rollup_config.l2_chain_id, run_context)?;

        // Create the directory for the rollup config if it doesn't exist.
        let rollup_configs_dir = rollup_config_path.parent().unwrap();
        if !rollup_configs_dir.exists() {
            fs::create_dir_all(rollup_configs_dir)?;
        }

        // Write the rollup config to the file.
        let rollup_config_str = serde_json::to_string_pretty(&rollup_config)?;
        fs::write(rollup_config_path, rollup_config_str)?;

        Ok(rollup_config)
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
        let url = self.get_rpc_url(rpc_mode);
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
        let headers = stream::iter(start..=end)
            .map(|block_number| async move { self.get_l1_header(block_number.into()).await })
            .buffered(100)
            .collect::<Vec<Result<Header>>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

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
        let headers = self
            .fetch_headers_in_range(start_header.number, latest_header.number)
            .await?;

        Ok(headers)
    }

    /// Get the data directory for the given program type and run context.
    ///
    /// If the RunContext is Dev, prepend the workspace root.
    fn get_data_directory(
        &self,
        l2_chain_id: u64,
        l2_start_block: u64,
        l2_end_block: u64,
        multi_block: ProgramType,
    ) -> Result<String> {
        let mut data_directory = match multi_block {
            ProgramType::Single => {
                format!("data/{}/{}", l2_chain_id, l2_end_block)
            }
            ProgramType::Multi => {
                format!("data/{}/{}-{}", l2_chain_id, l2_start_block, l2_end_block)
            }
        };

        // If the run context is Dev, prepend the workspace root.
        match self.run_context {
            RunContext::Dev => {
                let metadata = MetadataCommand::new().exec().unwrap();
                let workspace_root = metadata.workspace_root;
                data_directory = format!("{}/{}", workspace_root, data_directory);
                Ok(data_directory)
            }
            RunContext::Docker => {
                data_directory = format!("/usr/local/{}", data_directory);
                Ok(data_directory)
            }
        }
    }

    /// Get the exec directory for the given program type and run context.
    fn get_exec_directory(&self, multi_block: ProgramType) -> Result<String> {
        let exec_directory = match multi_block {
            ProgramType::Single => "fault-proof",
            ProgramType::Multi => "range",
        };

        // If the run context is Dev, prepend the workspace root.
        match self.run_context {
            RunContext::Dev => {
                let metadata = MetadataCommand::new().exec().unwrap();
                let workspace_root = metadata.workspace_root;
                Ok(format!(
                    "{}/target/release-client-lto/{}",
                    workspace_root, exec_directory
                ))
            }
            RunContext::Docker => Ok(format!("/usr/local/bin/{}", exec_directory)),
        }
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
        // If the rollup config is not already loaded, fetch and save it.
        if self.rollup_config.is_none() {
            return Err(anyhow::anyhow!("Rollup config not loaded."));
        }
        let l2_chain_id = self.rollup_config.as_ref().unwrap().l2_chain_id;

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
            .get_block_by_number(l2_start_block.into(), BlockTransactionsKind::Hashes)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("Block not found for block number {}", l2_start_block)
            })?;
        let l2_output_state_root = l2_output_block.header.state_root;
        let agreed_l2_head_hash = l2_output_block.header.hash;
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
            l2_claim_hash: agreed_l2_head_hash.0.into(),
        };
        let agreed_l2_output_root = keccak256(l2_output_encoded.abi_encode());

        // Get L2 claim data.
        let l2_claim_block = l2_provider
            .get_block_by_number(l2_end_block.into(), BlockTransactionsKind::Hashes)
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
        let claimed_l2_output_root = keccak256(l2_claim_encoded.abi_encode());

        let (l1_head_hash, _) = self.get_l1_head(l2_end_block).await?;

        // Get the workspace root, which is where the data directory is.
        let data_directory =
            self.get_data_directory(l2_chain_id, l2_start_block, l2_end_block, multi_block)?;

        // The native programs are built with profile release-client-lto in build.rs
        let exec_directory = self.get_exec_directory(multi_block)?;

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
        let rollup_config_path = get_rollup_config_path(l2_chain_id, self.run_context)?;

        // Creates the data directory if it doesn't exist, or no-ops if it does. Used to store the
        // witness data.
        fs::create_dir_all(&data_directory)?;

        Ok(HostCli {
            l1_head: l1_head_hash,
            agreed_l2_output_root,
            agreed_l2_head_hash,
            claimed_l2_output_root,
            claimed_l2_block_number: l2_end_block,
            l2_chain_id: None,
            // Trim the trailing slash to avoid double slashes in the URL.
            l2_node_address: Some(
                self.rpc_config
                    .l2_rpc
                    .as_str()
                    .trim_end_matches('/')
                    .to_string(),
            ),
            l1_node_address: Some(
                self.rpc_config
                    .l1_rpc
                    .as_str()
                    .trim_end_matches('/')
                    .to_string(),
            ),
            l1_beacon_address: Some(
                self.rpc_config
                    .l1_beacon_rpc
                    .as_str()
                    .trim_end_matches('/')
                    .to_string(),
            ),
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
    #[allow(dead_code)]
    async fn get_l1_block_time(&self) -> Result<u64> {
        let l1_head = self.get_l1_header(BlockId::latest()).await?;

        let l1_head_minus_1 = l1_head.number - 1;
        let l1_block_minus_1 = self.get_l1_header(l1_head_minus_1.into()).await?;
        Ok(l1_head.timestamp - l1_block_minus_1.timestamp)
    }

    /// Get the L2 block time in seconds.
    pub async fn get_l2_block_time(&self) -> Result<u64> {
        let l2_head = self.get_l2_header(BlockId::latest()).await?;

        let l2_head_minus_1 = l2_head.number - 1;
        let l2_block_minus_1 = self.get_l2_header(l2_head_minus_1.into()).await?;
        Ok(l2_head.timestamp - l2_block_minus_1.timestamp)
    }

    /// Get the L1 block from which the `l2_end_block` can be derived.
    pub async fn get_l1_head_with_safe_head(&self, l2_end_block: u64) -> Result<(B256, u64)> {
        let latest_l1_header = self.get_l1_header(BlockId::latest()).await?;

        // Get the l1 origin of the l2 end block.
        let l2_end_block_hex = format!("0x{:x}", l2_end_block);
        let optimism_output_data: OutputResponse = self
            .fetch_rpc_data_with_mode(
                RPCMode::L2Node,
                "optimism_outputAtBlock",
                vec![l2_end_block_hex.into()],
            )
            .await?;

        let l1_origin = optimism_output_data.block_ref.l1_origin;

        // Search forward from the l1Origin, checking each L1 block until we find one with an L2 safe head greater than l2_end_block
        let mut current_l1_block_number = l1_origin.number;
        loop {
            // If the current L1 block number is greater than the latest L1 header number, then return an error.
            if current_l1_block_number > latest_l1_header.number {
                return Err(anyhow::anyhow!(
                    "Could not find an L1 block with an L2 safe head greater than the L2 end block."
                ));
            }

            let l1_block_number_hex = format!("0x{:x}", current_l1_block_number);
            let result: SafeHeadResponse = self
                .fetch_rpc_data_with_mode(
                    RPCMode::L2Node,
                    "optimism_safeHeadAtL1Block",
                    vec![l1_block_number_hex.into()],
                )
                .await?;
            let l2_safe_head = result.safe_head.number;
            // If the safe head is greater than or equal to the L2 end block at this L1 block, then we can derive the L2 end block from this L1 block.
            if l2_safe_head >= l2_end_block {
                return Ok((result.l1_block.hash, result.l1_block.number));
            }

            current_l1_block_number += 1;
        }
    }

    /// For OP Sepolia, OP Mainnet and Base, the batcher posts at least every 10 minutes. Otherwise,
    /// the batcher may post as infrequently as every couple hours. The l1Head is set as the l1 block from which all of the
    /// relevant L2 block data can be derived.
    /// E.g. Origin Advance Error: BlockInfoFetch(Block number past L1 head.).
    async fn get_l1_head(&self, l2_end_block: u64) -> Result<(B256, u64)> {
        // If the rollup config is not already loaded, fetch and save it.
        if self.rollup_config.is_none() {
            return Err(anyhow::anyhow!("Rollup config not loaded."));
        }
        let l2_chain_id = self.rollup_config.as_ref().unwrap().l2_chain_id;

        // See if optimism_safeHeadAtL1Block is available. If there's an error, then estimate the L1 block necessary based on the chain config.
        let result = self.get_l1_head_with_safe_head(l2_end_block).await;

        if let Ok(safe_head_at_l1_block) = result {
            Ok(safe_head_at_l1_block)
        } else {
            // Estimate the L1 block necessary based on the chain config. This is based on the maximum
            // delay between batches being posted on the L2 chain.
            let max_batch_post_delay_minutes = match l2_chain_id {
                11155420 => 10,
                10 => 10,
                8453 => 10,
                _ => 60,
            };

            // Get L1 head.
            let l2_block_timestamp = self.get_l2_header(l2_end_block.into()).await?.timestamp;
            let latest_l1_timestamp = self.get_l1_header(BlockId::latest()).await?.timestamp;

            // Ensure that the target timestamp is not greater than the latest L1 timestamp.
            let target_timestamp = min(
                l2_block_timestamp + (max_batch_post_delay_minutes * 60),
                latest_l1_timestamp,
            );
            Ok(self.find_l1_block_by_timestamp(target_timestamp).await?)
        }
    }

    // Source from: https://github.com/anton-rs/kona/blob/85b1c88b44e5f54edfc92c781a313717bad5dfc7/crates/derive-alloy/src/alloy_providers.rs#L225.
    pub async fn get_l2_block_by_number(&self, block_number: u64) -> Result<OpBlock> {
        let raw_block: Bytes = self
            .l2_provider
            .raw_request("debug_getRawBlock".into(), [U64::from(block_number)])
            .await?;
        let block = OpBlock::decode(&mut raw_block.as_ref()).map_err(|e| anyhow::anyhow!(e))?;
        Ok(block)
    }

    pub async fn l2_block_info_by_number(&self, block_number: u64) -> Result<L2BlockInfo> {
        // If the rollup config is not already loaded, fetch and save it.
        if self.rollup_config.is_none() {
            return Err(anyhow::anyhow!("Rollup config not loaded."));
        }
        let genesis = self.rollup_config.as_ref().unwrap().genesis;
        let block = self.get_l2_block_by_number(block_number).await?;
        Ok(L2BlockInfo::from_block_and_genesis(&block, &genesis)?)
    }

    /// Get the L2 safe head corresponding to the L1 block number using optimism_safeHeadAtL1Block.
    pub async fn get_l2_safe_head_from_l1_block_number(&self, l1_block_number: u64) -> Result<u64> {
        let l1_block_number_hex = format!("0x{:x}", l1_block_number);
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
        let l1_block = self.get_l1_header(BlockId::latest()).await?;
        let l1_block_number_hex = format!("0x{:x}", l1_block.number);
        let result: Result<SafeHeadResponse, _> = self
            .fetch_rpc_data_with_mode(
                RPCMode::L2Node,
                "optimism_safeHeadAtL1Block",
                vec![l1_block_number_hex.into()],
            )
            .await;
        Ok(result.is_ok())
    }

    /// Get the l2_end_block number given the l2_start_block number and the ideal block interval.
    /// Picks the l2 end block that minimizes the derivation cost by picking the l2 block that can be derived from the same batch as the l2_start_block.
    pub async fn get_l2_end_block(
        &self,
        l2_start_block: u64,
        ideal_block_interval: u64,
    ) -> Result<u64> {
        let ideal_l2_block_end = l2_start_block + ideal_block_interval;
        let l2_end_block_info = self.l2_block_info_by_number(ideal_l2_block_end).await?;

        let l2_derivable_block_end = self
            .get_l2_safe_head_from_l1_block_number(l2_end_block_info.l1_origin.number)
            .await?;

        // If blocks are in same batch or if derivable end is past ideal end, use ideal end block, as it will just pull in one batch.
        if l2_derivable_block_end < l2_start_block || l2_derivable_block_end > ideal_l2_block_end {
            Ok(ideal_l2_block_end)
        } else {
            // Otherwise use derivable end to avoid pulling in multiple batches.
            Ok(l2_derivable_block_end)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::fetcher::OPSuccinctDataFetcher;
    use crate::fetcher::RunContext;

    #[tokio::test]
    #[cfg(test)]
    async fn test_get_l1_head() {
        use alloy::eips::BlockId;

        dotenv::dotenv().ok();
        let fetcher = OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Dev)
            .await
            .unwrap();
        let latest_l2_block = fetcher.get_l2_header(BlockId::latest()).await.unwrap();

        // Get the L2 block number from 1 hour ago.
        let l2_end_block = latest_l2_block.number
            - ((60 * 60) / fetcher.rollup_config.as_ref().unwrap().block_time);

        let _ = fetcher.get_l1_head(l2_end_block).await.unwrap();
    }

    #[tokio::test]
    #[cfg(test)]
    async fn test_l2_safe_head_progression() {
        use alloy::eips::BlockId;
        use futures::StreamExt;
        use op_alloy_rpc_types::SafeHeadResponse;

        use crate::fetcher::RPCMode;

        dotenv::dotenv().ok();
        let fetcher = OPSuccinctDataFetcher::new(RunContext::Dev);
        let mut l2_safe_heads = Vec::new();

        let latest_l1_block = fetcher.get_l1_header(BlockId::latest()).await.unwrap();
        let latest_l1_block_number = latest_l1_block.number;
        let l1_block_number_hex = format!("0x{:x}", latest_l1_block_number);
        let _ = fetcher
            .fetch_rpc_data_with_mode::<SafeHeadResponse>(
                RPCMode::L2Node,
                "optimism_safeHeadAtL1Block",
                vec![l1_block_number_hex.into()],
            )
            .await
            .unwrap();

        let latest_l2_block = fetcher.get_l2_header(BlockId::latest()).await.unwrap();

        let safe_heads =
            futures::stream::iter(latest_l2_block.number - 500..=latest_l2_block.number)
                .map(|block_num| {
                    let l1_block_number_hex = format!("0x{:x}", block_num);
                    fetcher.fetch_rpc_data_with_mode::<SafeHeadResponse>(
                        RPCMode::L2Node,
                        "optimism_safeHeadAtL1Block",
                        vec![l1_block_number_hex.into()],
                    )
                })
                .buffered(100)
                .collect::<Vec<_>>()
                .await;

        for result in safe_heads.into_iter().flatten() {
            l2_safe_heads.push(result.safe_head.number);
        }
    }
}
