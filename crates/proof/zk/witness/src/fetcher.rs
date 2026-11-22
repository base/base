use std::{
    env, fmt, fs,
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_consensus::{EMPTY_ROOT_HASH, Header};
use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, keccak256};
use alloy_provider::{
    Provider, ProviderBuilder, RootProvider, network::primitives::HeaderResponse,
};
use alloy_sol_types::SolValue;
use anyhow::{Context, Result, bail};
use base_common_consensus::Predeploys;
use base_common_genesis::{BaseFeature, RollupConfig};
use base_common_network::Base;
use base_proof_host::HostConfig;
use base_proof_zk_utils::boot::BootInfoStruct;
use futures::{StreamExt, stream};
use reqwest::Url;
use serde_json::{Value, json};

use crate::L2Output;

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

/// RPC endpoint URLs for L1 and L2.
#[derive(Debug, Clone)]
pub struct RPCConfig {
    /// L1 execution RPC URL.
    pub l1_rpc: Url,
    /// L1 beacon RPC URL (optional).
    pub l1_beacon_rpc: Option<Url>,
    /// L2 execution RPC URL.
    pub l2_rpc: Url,
    /// L2 consensus node RPC URL.
    pub l2_node_rpc: Url,
    /// Directory containing `<chain_id>.json` L1 chain configs.
    ///
    /// When unset, [`Self::l1_config_directory`] falls back to `L1_CONFIG_DIR`, then `configs/L1`.
    pub l1_config_dir: Option<PathBuf>,
    /// Directory where fetched L2 rollup configs are written.
    ///
    /// When unset, [`Self::l2_config_directory`] falls back to `L2_CONFIG_DIR`, then `configs/L2`.
    pub l2_config_dir: Option<PathBuf>,
}

impl RPCConfig {
    /// Directory containing `<chain_id>.json` L1 chain configs.
    pub fn l1_config_directory(&self) -> PathBuf {
        resolve_config_dir(self.l1_config_dir.as_deref(), "L1_CONFIG_DIR", "configs/L1")
    }

    /// Directory where fetched L2 rollup configs are written.
    pub fn l2_config_directory(&self) -> PathBuf {
        resolve_config_dir(self.l2_config_dir.as_deref(), "L2_CONFIG_DIR", "configs/L2")
    }
}

fn resolve_config_dir(explicit: Option<&Path>, env_name: &str, default: &str) -> PathBuf {
    explicit.map(Path::to_path_buf).unwrap_or_else(|| {
        env::var_os(env_name).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(default))
    })
}

impl OPSuccinctDataFetcher {
    /// Initialize the fetcher with an explicit [`RPCConfig`] and a rollup config.
    pub async fn from_rpc_config_with_rollup_config(rpc_config: RPCConfig) -> Result<Self> {
        let l1_provider =
            Arc::new(ProviderBuilder::default().connect_http(rpc_config.l1_rpc.clone()));
        let l2_provider =
            Arc::new(ProviderBuilder::default().connect_http(rpc_config.l2_rpc.clone()));

        let (rollup_config, rollup_config_path) =
            Self::fetch_and_save_rollup_config(&rpc_config).await?;

        let unix_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        if !rollup_config
            .is_feature_active_cascading(BaseFeature::StrictDerivationOrdering, unix_timestamp)
        {
            tracing::warn!(
                "Chain is not using Holocene upgrade. This will cause significant performance degradation compared to chains that have activated Holocene."
            );
        }

        let l1_config_path = Self::fetch_and_save_l1_config(&rollup_config, &rpc_config).await?;

        Ok(Self {
            rpc_config,
            l1_provider,
            l2_provider,
            rollup_config: Some(rollup_config),
            rollup_config_path: Some(rollup_config_path),
            l1_config_path: Some(l1_config_path),
        })
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

    /// Fetch and save the rollup config to a temporary file.
    async fn fetch_and_save_rollup_config(
        rpc_config: &RPCConfig,
    ) -> Result<(RollupConfig, PathBuf)> {
        let rollup_config: RollupConfig =
            Self::fetch_rpc_data(&rpc_config.l2_node_rpc, "optimism_rollupConfig", vec![]).await?;

        let l2_config_dir = rpc_config.l2_config_directory();
        fs::create_dir_all(&l2_config_dir)?;

        let rollup_config_path = l2_config_dir.join(format!("{}.json", rollup_config.l2_chain_id));

        let rollup_config_str = serde_json::to_string_pretty(&rollup_config)?;
        fs::write(&rollup_config_path, rollup_config_str)?;

        tracing::info!(
            "Saved L2 config for chain ID {} to {}",
            rollup_config.l2_chain_id,
            rollup_config_path.display()
        );

        Ok((rollup_config, rollup_config_path))
    }

    /// Fetch and save the L1 config based on the rollup config's L1 chain ID.
    async fn fetch_and_save_l1_config(
        rollup_config: &RollupConfig,
        rpc_config: &RPCConfig,
    ) -> Result<PathBuf> {
        let l1_config_dir = rpc_config.l1_config_directory();

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

        fs::create_dir_all(&l1_config_dir)
            .with_context(|| format!("creating {}", l1_config_dir.display()))?;

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

        if let Some(error) = response.get("error") {
            let error_message = error["message"].as_str().unwrap_or("Unknown error");
            return Err(anyhow::anyhow!("Error calling {method}: {error_message}"));
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
        let start_header = self.get_earliest_l1_head_in_batch(boot_infos).await?;

        // Fetch the full header for the latest L1 Head (which is validated on chain).
        let latest_header = self.get_l1_header(checkpoint_block_hash.into()).await?;

        let headers =
            self.fetch_headers_in_range(start_header.number, latest_header.number).await?;

        Ok(headers)
    }

    /// Get the L2 output data for a given block number and save the boot info to a file in the data
    /// directory with `block_number`. Return the arguments to be passed to the native host for
    /// datagen.
    pub async fn get_host_args(
        &self,
        l2_start_block: u64,
        l2_end_block: u64,
        l1_head_hash: B256,
        schedule_l2_block_number: Option<u64>,
        intermediate_block_interval: NonZeroU64,
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

        let l2_output_block = l2_provider
            .get_block_by_number(l2_start_block.into())
            .await
            .with_context(|| format!("fetch L2 start block {l2_start_block}"))?
            .ok_or_else(|| anyhow::anyhow!("Block not found for block number {l2_start_block}"))?;
        let l2_output_state_root = l2_output_block.header.state_root;
        let agreed_l2_head_hash = l2_output_block.header.hash;
        let l2_output_storage_hash = self
            .l2_to_l1_message_passer_storage_root(rollup_config, &l2_output_block.header)
            .await?;

        let l2_output_encoded = L2Output {
            zero: 0,
            l2_state_root: l2_output_state_root.0.into(),
            l2_storage_hash: l2_output_storage_hash.0.into(),
            l2_claim_hash: agreed_l2_head_hash.0.into(),
        };
        let agreed_l2_output_root = keccak256(l2_output_encoded.abi_encode());

        let l2_claim_block = l2_provider
            .get_block_by_number(l2_end_block.into())
            .await
            .with_context(|| format!("fetch L2 end block {l2_end_block}"))?
            .ok_or_else(|| anyhow::anyhow!("Block not found for block number {l2_end_block}"))?;
        let l2_claim_state_root = l2_claim_block.header.state_root;
        let l2_claim_hash = l2_claim_block.header.hash;
        let l2_claim_storage_hash = self
            .l2_to_l1_message_passer_storage_root(rollup_config, &l2_claim_block.header)
            .await?;

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

        let l1_config = if let Some(ref l1_config_path) = self.l1_config_path {
            let file = fs::File::open(l1_config_path)
                .with_context(|| format!("open L1 config {}", l1_config_path.display()))?;
            serde_json::from_reader(file)
                .with_context(|| format!("deserialize L1 config {}", l1_config_path.display()))?
        } else {
            base_common_chains::L1_CONFIGS
                .get(&rollup_config.l1_chain_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("No L1 config for chain ID {}", rollup_config.l1_chain_id)
                })?
                .clone()
        };

        let l1_head_number = self
            .get_l1_header(l1_head_hash.into())
            .await
            .with_context(|| format!("fetch L1 header {l1_head_hash}"))?
            .number;

        let request = base_proof_primitives::ProofRequest {
            l1_head: l1_head_hash,
            agreed_l2_output_root,
            agreed_l2_head_hash,
            claimed_l2_output_root,
            claimed_l2_block_number: l2_end_block,
            l1_head_number,
            // We don't need to set the proposer for the range proof zk program
            proposer: Address::ZERO,
            schedule_l2_block_number,
            intermediate_block_interval,
        };

        let prover = base_proof_host::ProverConfig {
            l1_eth_url: self.rpc_config.l1_rpc.as_str().trim_end_matches('/').to_string(),
            l2_eth_url: self.rpc_config.l2_rpc.as_str().trim_end_matches('/').to_string(),
            l2_node_url: self.rpc_config.l2_node_rpc.as_str().trim_end_matches('/').to_string(),
            l1_beacon_url,
            l2_chain_id: rollup_config.l2_chain_id.id(),
            rollup_config: rollup_config.clone(),
            l1_config,
            enable_experimental_witness_endpoint: true,
        };

        Ok(HostConfig { request, prover, data_dir: None })
    }

    /// Returns the storage root of the L2-to-L1 message passer at `header`.
    ///
    /// Returns `header.withdrawals_root()` without an RPC call when Isthmus is active at
    /// `header.timestamp()` and the root is present and not [`EMPTY_ROOT_HASH`]. Otherwise
    /// queries `eth_getProof` pinned to `header.hash()`, so the result cannot come from a
    /// different block at the same height.
    pub async fn l2_to_l1_message_passer_storage_root(
        &self,
        rollup_config: &RollupConfig,
        header: &impl HeaderResponse,
    ) -> Result<B256> {
        if rollup_config.is_isthmus_active(header.timestamp())
            && let Some(root) = header.withdrawals_root()
            && root != EMPTY_ROOT_HASH
        {
            return Ok(root);
        }

        let block_hash = header.hash();
        Ok(self
            .l2_provider
            .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Vec::new())
            .block_id(block_hash.into())
            .await
            .with_context(|| {
                format!(
                    "eth_getProof L2ToL1MessagePasser at L2 block {} ({block_hash})",
                    header.number()
                )
            })?
            .storage_hash)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_eips::BlockNumberOrTag;
    use alloy_json_rpc::{RequestPacket, ResponsePacket};
    use alloy_provider::transport::{TransportError, TransportFut};
    use alloy_rpc_client::RpcClient;
    use base_common_genesis::UpgradeConfig;
    use base_protocol::OutputRoot;
    use tower::{ServiceExt, service_fn};

    use super::*;

    const ISTHMUS_TIME: u64 = 100;
    const PROOF_STORAGE_HASH: B256 = B256::repeat_byte(0xCD);

    fn rpc_block(header: &Header) -> Value {
        let mut block = serde_json::to_value(header).unwrap();
        block["hash"] = json!(header.hash_slow());
        block["transactions"] = json!([]);
        block["uncles"] = json!([]);
        block
    }

    /// Serves `blocks` by number or hash, answers every `eth_getProof` with
    /// [`PROOF_STORAGE_HASH`], and records the block each proof was requested at.
    fn scripted_client(blocks: Vec<Header>, proof_blocks: Arc<Mutex<Vec<BlockId>>>) -> RpcClient {
        let transport = service_fn(move |request: RequestPacket| {
            let blocks = blocks.clone();
            let proof_blocks = Arc::clone(&proof_blocks);
            async move {
                let RequestPacket::Single(request) = request else { panic!("unexpected batch") };
                let params: Value =
                    serde_json::from_str(request.params().map_or("[]", |p| p.get())).unwrap();
                let result = match request.method() {
                    "eth_getBlockByNumber" => {
                        let number: BlockNumberOrTag =
                            serde_json::from_value(params[0].clone()).unwrap();
                        rpc_block(
                            blocks.iter().find(|h| Some(h.number) == number.as_number()).unwrap(),
                        )
                    }
                    "eth_getBlockByHash" => {
                        let hash: B256 = serde_json::from_value(params[0].clone()).unwrap();
                        rpc_block(blocks.iter().find(|h| h.hash_slow() == hash).unwrap())
                    }
                    "eth_getProof" => {
                        proof_blocks
                            .lock()
                            .unwrap()
                            .push(serde_json::from_value(params[2].clone()).unwrap());
                        json!({
                            "address": Predeploys::L2_TO_L1_MESSAGE_PASSER,
                            "balance": "0x0",
                            "codeHash": B256::ZERO,
                            "nonce": "0x0",
                            "storageHash": PROOF_STORAGE_HASH,
                            "accountProof": [],
                            "storageProof": [],
                        })
                    }
                    method => panic!("unexpected RPC method {method}"),
                };
                let response = json!({ "jsonrpc": "2.0", "id": request.id(), "result": result });
                Ok::<_, TransportError>(serde_json::from_value::<ResponsePacket>(response).unwrap())
            }
        })
        .map_future(|future| Box::pin(future) as TransportFut<'static>);
        RpcClient::new(transport, true)
    }

    #[tokio::test]
    async fn host_args_read_message_passer_root_from_header_after_isthmus() {
        let isthmus_root = B256::repeat_byte(0xEE);
        let cases = [
            ("isthmus root", Some(isthmus_root), isthmus_root, 1),
            ("empty root", Some(EMPTY_ROOT_HASH), PROOF_STORAGE_HASH, 2),
            ("missing root", None, PROOF_STORAGE_HASH, 2),
        ];

        for (name, end_withdrawals_root, expected_end_storage, expected_proofs) in cases {
            // The start block is pre-Isthmus, so its withdrawals_root must be ignored.
            let start = Header {
                number: 5,
                timestamp: ISTHMUS_TIME - 2,
                state_root: B256::repeat_byte(0x01),
                withdrawals_root: Some(B256::repeat_byte(0xAB)),
                ..Default::default()
            };
            let end = Header {
                number: 7,
                timestamp: ISTHMUS_TIME,
                state_root: B256::repeat_byte(0x02),
                withdrawals_root: end_withdrawals_root,
                ..Default::default()
            };
            let l1_head = Header { number: 1_000, ..Default::default() };
            let (start_hash, end_hash, l1_head_hash) =
                (start.hash_slow(), end.hash_slow(), l1_head.hash_slow());

            let proof_blocks = Arc::new(Mutex::new(Vec::new()));
            let client = scripted_client(
                vec![start.clone(), end.clone(), l1_head],
                Arc::clone(&proof_blocks),
            );
            let fetcher = OPSuccinctDataFetcher {
                rpc_config: rpc_config(None, None),
                l1_provider: Arc::new(RootProvider::new(client.clone())),
                l2_provider: Arc::new(RootProvider::new(client)),
                rollup_config: Some(RollupConfig {
                    l1_chain_id: 1,
                    upgrades: UpgradeConfig {
                        isthmus_time: Some(ISTHMUS_TIME),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                rollup_config_path: None,
                l1_config_path: None,
            };

            let host_args =
                fetcher.get_host_args(5, 7, l1_head_hash, None, NonZeroU64::MIN).await.unwrap();

            assert_eq!(
                host_args.request.agreed_l2_output_root,
                OutputRoot::from_parts(start.state_root, PROOF_STORAGE_HASH, start_hash).hash(),
                "{name}"
            );
            assert_eq!(
                host_args.request.claimed_l2_output_root,
                OutputRoot::from_parts(end.state_root, expected_end_storage, end_hash).hash(),
                "{name}"
            );
            let expected_proof_blocks: Vec<BlockId> =
                [start_hash, end_hash].into_iter().take(expected_proofs).map(Into::into).collect();
            assert_eq!(*proof_blocks.lock().unwrap(), expected_proof_blocks, "{name}");
        }
    }

    fn rpc_config(l1: Option<PathBuf>, l2: Option<PathBuf>) -> RPCConfig {
        RPCConfig {
            l1_rpc: "http://l1".parse().unwrap(),
            l1_beacon_rpc: None,
            l2_rpc: "http://l2".parse().unwrap(),
            l2_node_rpc: "http://l2-node".parse().unwrap(),
            l1_config_dir: l1,
            l2_config_dir: l2,
        }
    }

    #[test]
    fn explicit_config_dirs_win_over_defaults() {
        let rpc = rpc_config(Some(PathBuf::from("/tmp/l1")), Some(PathBuf::from("/tmp/l2")));
        assert_eq!(rpc.l1_config_directory(), PathBuf::from("/tmp/l1"));
        assert_eq!(rpc.l2_config_directory(), PathBuf::from("/tmp/l2"));
    }
}
