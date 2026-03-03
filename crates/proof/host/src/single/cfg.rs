//! This module contains all CLI-specific code for the single chain entrypoint.

use std::{path::PathBuf, sync::Arc};

use alloy_primitives::B256;
use alloy_provider::RootProvider;
use base_alloy_network::Base;
use base_consensus_genesis::{L1ChainConfig, RollupConfig};
use base_consensus_providers::{OnlineBeaconClient, OnlineBlobProvider};
use base_proof::HintType;
use base_proof_preimage::{BidirectionalChannel, Channel, HintReader, OracleServer};
use base_proof_std_fpvm::{FileChannel, FileDescriptor};
use clap::Parser;
use serde::Serialize;
use tokio::{
    sync::RwLock,
    task::{self, JoinHandle},
};

use super::{SingleChainHintHandler, SingleChainLocalInputs};
use crate::{
    DiskKeyValueStore, MemoryKeyValueStore, OfflineHostBackend, OnlineHostBackend,
    OnlineHostBackendCfg, PreimageServer, PreimageServerError, SharedKeyValueStore,
    SplitKeyValueStore, rpc_provider,
};

/// The host binary CLI application arguments.
#[derive(Default, Parser, Serialize, Clone, Debug)]
pub struct SingleChainHost {
    /// Hash of the L1 head block. Derivation stops after this block is processed.
    #[arg(long, env)]
    pub l1_head: B256,
    /// Hash of the agreed upon safe L2 block committed to by `--agreed-l2-output-root`.
    #[arg(long, visible_alias = "l2-head", env)]
    pub agreed_l2_head_hash: B256,
    /// Agreed safe L2 Output Root to start derivation from.
    #[arg(long, visible_alias = "l2-output-root", env)]
    pub agreed_l2_output_root: B256,
    /// Claimed L2 output root at block # `--claimed-l2-block-number` to validate.
    #[arg(long, visible_alias = "l2-claim", env)]
    pub claimed_l2_output_root: B256,
    /// Number of the L2 block that the claimed output root commits to.
    #[arg(long, visible_alias = "l2-block-number", env)]
    pub claimed_l2_block_number: u64,
    /// Address of L2 JSON-RPC endpoint to use (eth and debug namespace required).
    #[arg(
        long,
        visible_alias = "l2",
        requires = "l1_node_address",
        requires = "l1_beacon_address",
        env
    )]
    pub l2_node_address: Option<String>,
    /// Address of L1 JSON-RPC endpoint to use (eth and debug namespace required)
    #[arg(
        long,
        visible_alias = "l1",
        requires = "l2_node_address",
        requires = "l1_beacon_address",
        env
    )]
    pub l1_node_address: Option<String>,
    /// Address of the L1 Beacon API endpoint to use.
    #[arg(
        long,
        visible_alias = "beacon",
        requires = "l1_node_address",
        requires = "l2_node_address",
        env
    )]
    pub l1_beacon_address: Option<String>,
    /// The Data Directory for preimage data storage. Optional if running in online mode,
    /// required if running in offline mode.
    #[arg(
        long,
        visible_alias = "db",
        required_unless_present_all = ["l2_node_address", "l1_node_address", "l1_beacon_address"],
        env
    )]
    pub data_dir: Option<PathBuf>,
    /// Run the client program natively.
    #[arg(long, conflicts_with = "server", required_unless_present = "server")]
    pub native: bool,
    /// Run in pre-image server mode without executing any client program.
    #[arg(long, conflicts_with = "native", required_unless_present = "native")]
    pub server: bool,
    /// The L2 chain ID of a supported chain.
    #[arg(
        long,
        conflicts_with = "rollup_config_path",
        required_unless_present = "rollup_config_path",
        env
    )]
    pub l2_chain_id: Option<u64>,
    /// Path to rollup config.
    #[arg(
        long,
        alias = "rollup-cfg",
        conflicts_with = "l2_chain_id",
        required_unless_present = "l2_chain_id",
        env
    )]
    pub rollup_config_path: Option<PathBuf>,
    /// Path to l1 config.
    #[arg(long, alias = "l1-cfg", env)]
    pub l1_config_path: Option<PathBuf>,
    /// Optionally enables the use of `debug_executePayload` to collect the execution witness.
    #[arg(long, env)]
    pub enable_experimental_witness_endpoint: bool,
}

/// An error that can occur when handling single chain hosts
#[derive(Debug, thiserror::Error)]
pub enum SingleChainHostError {
    /// An error when handling preimage requests.
    #[error("Error handling preimage request: {0}")]
    PreimageServerError(#[from] PreimageServerError),
    /// An IO error.
    #[error("IO error: {0}")]
    IOError(#[from] std::io::Error),
    /// A JSON parse error.
    #[error("Failed deserializing RollupConfig: {0}")]
    ParseError(#[from] serde_json::Error),
    /// Task failed to execute to completion.
    #[error("Join error: {0}")]
    ExecutionError(#[from] tokio::task::JoinError),
    /// No rollup config found.
    #[error("No rollup config found")]
    NoRollupConfig,
    /// No l1 config found.
    #[error("No l1 config found")]
    NoL1Config,
    /// Any other error.
    #[error("Error: {0}")]
    Other(&'static str),
}

impl SingleChainHost {
    /// Starts the [`SingleChainHost`] application.
    pub async fn start(self) -> Result<(), SingleChainHostError> {
        if self.server {
            let hint = FileChannel::new(FileDescriptor::HintRead, FileDescriptor::HintWrite);
            let preimage =
                FileChannel::new(FileDescriptor::PreimageRead, FileDescriptor::PreimageWrite);

            self.start_server(hint, preimage).await?.await?
        } else {
            self.start_native().await
        }
    }

    /// Starts the preimage server, communicating with the client over the provided channels.
    pub async fn start_server<C>(
        &self,
        hint: C,
        preimage: C,
    ) -> Result<JoinHandle<Result<(), SingleChainHostError>>, SingleChainHostError>
    where
        C: Channel + Send + Sync + 'static,
    {
        let kv_store = self.create_key_value_store()?;

        let task_handle = if self.is_offline() {
            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(OfflineHostBackend::new(kv_store)),
                )
                .start()
                .await
                .map_err(SingleChainHostError::from)
            })
        } else {
            let providers = self.create_providers().await?;
            let backend = OnlineHostBackend::new(
                self.clone(),
                Arc::clone(&kv_store),
                providers,
                SingleChainHintHandler,
            )
            .with_proactive_hint(HintType::L2PayloadWitness);

            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(backend),
                )
                .start()
                .await
                .map_err(SingleChainHostError::from)
            })
        };

        Ok(task_handle)
    }

    /// Starts the host in native mode, running both the client and preimage server in the same
    /// process.
    async fn start_native(&self) -> Result<(), SingleChainHostError> {
        let hint = BidirectionalChannel::new()?;
        let preimage = BidirectionalChannel::new()?;

        let server_task = self.start_server(hint.host, preimage.host).await?;
        let client_task = task::spawn(async { Ok::<(), SingleChainHostError>(()) });

        let (_, _client_result) = tokio::try_join!(server_task, client_task)?;

        Ok(())
    }

    /// Returns `true` if the host is running in offline mode.
    pub const fn is_offline(&self) -> bool {
        self.l1_node_address.is_none()
            && self.l2_node_address.is_none()
            && self.l1_beacon_address.is_none()
            && self.data_dir.is_some()
    }

    /// Reads the [`RollupConfig`] from the file system and returns the deserialized configuration.
    pub fn read_rollup_config(&self) -> Result<RollupConfig, SingleChainHostError> {
        let path =
            self.rollup_config_path.as_ref().ok_or_else(|| SingleChainHostError::NoRollupConfig)?;

        let ser_config = std::fs::read_to_string(path)?;
        serde_json::from_str(&ser_config).map_err(SingleChainHostError::ParseError)
    }

    /// Reads the [`L1ChainConfig`] from the file system and returns the deserialized configuration.
    pub fn read_l1_config(&self) -> Result<L1ChainConfig, SingleChainHostError> {
        let path = self.l1_config_path.as_ref().ok_or_else(|| SingleChainHostError::NoL1Config)?;

        let ser_config = std::fs::read_to_string(path)?;
        serde_json::from_str(&ser_config).map_err(SingleChainHostError::ParseError)
    }

    /// Creates the key-value store for the host backend.
    pub fn create_key_value_store(&self) -> Result<SharedKeyValueStore, SingleChainHostError> {
        let local_kv_store = SingleChainLocalInputs::new(self.clone());

        let kv_store: SharedKeyValueStore = if let Some(ref data_dir) = self.data_dir {
            let disk_kv_store = DiskKeyValueStore::new(data_dir.clone());
            let split_kv_store = SplitKeyValueStore::new(local_kv_store, disk_kv_store);
            Arc::new(RwLock::new(split_kv_store))
        } else {
            let mem_kv_store = MemoryKeyValueStore::new();
            let split_kv_store = SplitKeyValueStore::new(local_kv_store, mem_kv_store);
            Arc::new(RwLock::new(split_kv_store))
        };

        Ok(kv_store)
    }

    /// Creates the providers required for the host backend.
    pub async fn create_providers(&self) -> Result<SingleChainProviders, SingleChainHostError> {
        let l1_provider = rpc_provider(
            self.l1_node_address
                .as_ref()
                .ok_or(SingleChainHostError::Other("Provider must be set"))?,
        )
        .await;
        let blob_provider = OnlineBlobProvider::init(OnlineBeaconClient::new_http(
            self.l1_beacon_address
                .clone()
                .ok_or(SingleChainHostError::Other("Beacon API URL must be set"))?,
        ))
        .await;
        let l2_provider = rpc_provider::<Base>(
            self.l2_node_address
                .as_ref()
                .ok_or(SingleChainHostError::Other("L2 node address must be set"))?,
        )
        .await;

        Ok(SingleChainProviders { l1: l1_provider, blobs: blob_provider, l2: l2_provider })
    }
}

impl OnlineHostBackendCfg for SingleChainHost {
    type HintType = HintType;
    type Providers = SingleChainProviders;
}

/// The providers required for the single chain host.
#[derive(Debug, Clone)]
pub struct SingleChainProviders {
    /// The L1 EL provider.
    pub l1: RootProvider,
    /// The L1 beacon node provider.
    pub blobs: OnlineBlobProvider<OnlineBeaconClient>,
    /// The L2 EL provider.
    pub l2: RootProvider<Base>,
}

#[cfg(test)]
mod test {
    use alloy_primitives::B256;
    use clap::{CommandFactory, Parser};

    use crate::single::SingleChainHost;

    /// Clear all env vars that clap would read via `#[arg(env)]` so tests are
    /// isolated from the host environment.
    fn clear_clap_env_vars() {
        for arg in SingleChainHost::command().get_arguments() {
            if let Some(env_var) = arg.get_env() {
                unsafe { std::env::remove_var(env_var) };
            }
        }
    }

    #[test]
    fn test_flags() {
        clear_clap_env_vars();

        let zero_hash_str = &B256::ZERO.to_string();
        let default_flags = [
            "single",
            "--l1-head",
            zero_hash_str,
            "--l2-head",
            zero_hash_str,
            "--l2-output-root",
            zero_hash_str,
            "--l2-claim",
            zero_hash_str,
            "--l2-block-number",
            "0",
        ];

        let cases = [
            (["--server", "--l2-chain-id", "0", "--data-dir", "dummy"].as_slice(), true),
            (["--server", "--rollup-config-path", "dummy", "--data-dir", "dummy"].as_slice(), true),
            (["--native", "--l2-chain-id", "0", "--data-dir", "dummy"].as_slice(), true),
            (["--native", "--rollup-config-path", "dummy", "--data-dir", "dummy"].as_slice(), true),
            (
                [
                    "--l1-node-address",
                    "dummy",
                    "--l2-node-address",
                    "dummy",
                    "--l1-beacon-address",
                    "dummy",
                    "--server",
                    "--l2-chain-id",
                    "0",
                ]
                .as_slice(),
                true,
            ),
            (
                [
                    "--server",
                    "--l2-chain-id",
                    "0",
                    "--data-dir",
                    "dummy",
                    "--enable-experimental-witness-endpoint",
                ]
                .as_slice(),
                true,
            ),
            (["--server", "--native", "--l2-chain-id", "0"].as_slice(), false),
            (["--l2-chain-id", "0", "--rollup-config-path", "dummy", "--server"].as_slice(), false),
            (["--server"].as_slice(), false),
            (["--native"].as_slice(), false),
            (["--rollup-config-path", "dummy"].as_slice(), false),
            (["--l2-chain-id", "0"].as_slice(), false),
            (["--l1-node-address", "dummy", "--server", "--l2-chain-id", "0"].as_slice(), false),
            (["--l2-node-address", "dummy", "--server", "--l2-chain-id", "0"].as_slice(), false),
            (["--l1-beacon-address", "dummy", "--server", "--l2-chain-id", "0"].as_slice(), false),
            ([].as_slice(), false),
        ];

        for (args_ext, valid) in cases {
            let args = default_flags.iter().chain(args_ext.iter()).copied().collect::<Vec<_>>();

            let parsed = SingleChainHost::try_parse_from(args);
            assert_eq!(parsed.is_ok(), valid);
        }
    }
}
