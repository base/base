//! System test stack orchestration and lifecycle management.

use std::path::PathBuf;

use alloy_network::Ethereum;
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::JwtSecret;
use base_common_network::Base;
use base_tx_forwarding::TxForwardingConfig;
use eyre::{Result, WrapErr};
use tempfile::TempDir;
use url::Url;

use crate::{
    BATCHER, BUILDER, SEQUENCER,
    l1::{L1ContainerConfig, L1Stack, L1StackConfig},
    l2::{L2ClientConsensusMode, L2ContainerConfig, L2Stack, L2StackConfig},
    setup::{L1GenesisOutput, L2DeploymentOutput, SetupContainer},
    system_config::StableSystemTestConfig,
};

const DEFAULT_L1_CHAIN_ID: u64 = 1337;
const DEFAULT_L2_CHAIN_ID: u64 = 84538453;
const DEFAULT_SLOT_DURATION: u64 = 2;

/// A complete L1+L2 stack for system tests.
pub struct SystemTestStack {
    _temp_dir: TempDir,
    l1_genesis: L1GenesisOutput,
    l2_deployment: L2DeploymentOutput,
    l1_stack: L1Stack,
    l2_stack: L2Stack,
}

impl std::fmt::Debug for SystemTestStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemTestStack")
            .field("l1_genesis", &self.l1_genesis)
            .field("l2_deployment", &self.l2_deployment)
            .finish_non_exhaustive()
    }
}

impl SystemTestStack {
    /// Returns a reference to the L1 stack.
    pub const fn l1_stack(&self) -> &L1Stack {
        &self.l1_stack
    }

    /// Returns a reference to the L2 stack.
    pub const fn l2_stack(&self) -> &L2Stack {
        &self.l2_stack
    }

    /// Returns the public RPC URL of the L1 Reth node.
    pub async fn l1_rpc_url(&self) -> Result<Url> {
        self.l1_stack.rpc_url().await
    }

    /// Returns the public RPC URL of the L2 builder node.
    pub fn l2_rpc_url(&self) -> Result<Url> {
        self.l2_stack().rpc_url()
    }

    /// Returns a reference to the L1 genesis output.
    pub const fn l1_genesis(&self) -> &L1GenesisOutput {
        &self.l1_genesis
    }

    /// Returns a reference to the L2 deployment output.
    pub const fn l2_deployment(&self) -> &L2DeploymentOutput {
        &self.l2_deployment
    }

    /// Returns the internal RPC URL of the L1 Reth node.
    pub fn l1_internal_rpc_url(&self) -> String {
        self.l1_stack.reth().internal_rpc_url()
    }

    /// Returns the internal beacon URL of the L1 Lighthouse beacon node.
    pub fn l1_internal_beacon_url(&self) -> String {
        self.l1_stack.beacon().internal_beacon_url()
    }

    /// Returns the L2 client's RPC URL.
    pub fn l2_client_rpc_url(&self) -> Result<Url> {
        self.l2_stack().client_rpc_url()
    }

    /// Returns an L1 provider with Ethereum network.
    pub async fn l1_provider(&self) -> Result<RootProvider<Ethereum>> {
        let url = self.l1_rpc_url().await?;
        let client = RpcClient::builder().http(url);
        Ok(RootProvider::<Ethereum>::new(client))
    }

    /// Returns an L2 builder provider with Base network.
    pub fn l2_builder_provider(&self) -> Result<RootProvider<Base>> {
        let url = self.l2_rpc_url()?;
        let client = RpcClient::builder().http(url);
        Ok(RootProvider::<Base>::new(client))
    }

    /// Returns an L2 client provider with Base network.
    pub fn l2_client_provider(&self) -> Result<RootProvider<Base>> {
        let url = self.l2_client_rpc_url()?;
        let client = RpcClient::builder().http(url);
        Ok(RootProvider::<Base>::new(client))
    }

    /// Returns all RPC URLs for this system test stack.
    pub async fn urls(&self) -> Result<crate::SystemTestUrls> {
        Ok(crate::SystemTestUrls {
            l1_rpc: self.l1_rpc_url().await?.to_string(),
            l2_builder_rpc: self.l2_rpc_url()?.to_string(),
            l2_client_rpc: self.l2_client_rpc_url()?.to_string(),
            l2_builder_consensus_rpc: self.l2_stack().builder_consensus_rpc_url().to_string(),
            l2_client_consensus_rpc: self.l2_stack().client_consensus_rpc_url().to_string(),
        })
    }
}

/// Builder for creating a new `SystemTestStack`.
#[derive(Debug, Default)]
pub struct SystemTestStackBuilder {
    l1_chain_id: Option<u64>,
    l2_chain_id: Option<u64>,
    slot_duration: Option<u64>,
    isthmus_activation_block: Option<u64>,
    base_azul_activation_block: Option<u64>,
    base_beryl_activation_block: Option<u64>,
    output_dir: Option<PathBuf>,
    stable_config: Option<StableSystemTestConfig>,
    tx_forwarding_config: Option<TxForwardingConfig>,
    verifier_l1_confs: u64,
    client_consensus_mode: L2ClientConsensusMode,
}

impl SystemTestStackBuilder {
    /// Creates a new `SystemTestStackBuilder` with default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the L1 chain ID.
    pub const fn with_l1_chain_id(mut self, chain_id: u64) -> Self {
        self.l1_chain_id = Some(chain_id);
        self
    }

    /// Sets the L2 chain ID.
    pub const fn with_l2_chain_id(mut self, chain_id: u64) -> Self {
        self.l2_chain_id = Some(chain_id);
        self
    }

    /// Sets the slot duration.
    pub const fn with_slot_duration(mut self, slot_duration: u64) -> Self {
        self.slot_duration = Some(slot_duration);
        self
    }

    /// Sets the L2 block number at which Isthmus activates.
    pub const fn with_isthmus_activation_block(mut self, block: u64) -> Self {
        self.isthmus_activation_block = Some(block);
        self
    }

    /// Sets the L2 block number at which Base Azul activates.
    pub const fn with_base_azul_activation_block(mut self, block: u64) -> Self {
        self.base_azul_activation_block = Some(block);
        self
    }

    /// Sets the L2 block number at which Base Beryl activates.
    pub const fn with_base_beryl_activation_block(mut self, block: u64) -> Self {
        self.base_beryl_activation_block = Some(block);
        self
    }

    /// Sets the output directory for generated system test files.
    pub fn with_output_dir(mut self, output_dir: PathBuf) -> Self {
        self.output_dir = Some(output_dir);
        self
    }

    /// Enables stable container names and ports matching docker-compose.yml.
    pub fn with_stable_config(mut self) -> Self {
        self.stable_config = Some(StableSystemTestConfig::standard());
        self
    }

    /// Enables transaction forwarding on the client node.
    /// When enabled, the client will forward transactions to the builder via
    /// the `base_insertValidatedTransaction` RPC endpoint.
    pub fn with_tx_forwarding(mut self, config: TxForwardingConfig) -> Self {
        self.tx_forwarding_config = Some(config);
        self
    }

    /// Sets the number of L1 blocks to keep distance from the L1 head for the
    /// client (validator) node's derivation pipeline.
    pub const fn with_verifier_l1_confs(mut self, confs: u64) -> Self {
        self.verifier_l1_confs = confs;
        self
    }

    /// Runs the L2 client consensus node in follow mode against the builder RPC.
    pub const fn with_follow_mode_client_consensus(mut self) -> Self {
        self.client_consensus_mode = L2ClientConsensusMode::Follow;
        self
    }

    /// Builds and starts the system test stack.
    pub async fn build(self) -> Result<SystemTestStack> {
        let l1_chain_id = self.l1_chain_id.unwrap_or(DEFAULT_L1_CHAIN_ID);
        let l2_chain_id = self.l2_chain_id.unwrap_or(DEFAULT_L2_CHAIN_ID);
        let slot_duration = self.slot_duration.unwrap_or(DEFAULT_SLOT_DURATION);

        let temp_dir = TempDir::new().wrap_err("Failed to create temp directory")?;
        let output_dir = self.output_dir.unwrap_or_else(|| temp_dir.path().to_path_buf());

        let mut setup = SetupContainer::new(&output_dir)
            .with_chain_id(l1_chain_id)
            .with_l2_chain_id(l2_chain_id)
            .with_slot_duration(slot_duration);

        if let Some(block) = self.isthmus_activation_block {
            setup = setup.with_isthmus_activation_block(block);
        }

        if let Some(block) = self.base_azul_activation_block {
            setup = setup.with_base_azul_activation_block(block);
        }

        if let Some(block) = self.base_beryl_activation_block {
            setup = setup.with_base_beryl_activation_block(block);
        }

        if let Some(ref config) = self.stable_config {
            setup = setup.with_network_name(&config.network_name);
        }

        let l1_genesis = tokio::task::spawn_blocking({
            let setup = setup.clone();
            move || setup.generate_l1_genesis()
        })
        .await
        .wrap_err("L1 genesis task panicked")?
        .wrap_err("Failed to generate L1 genesis")?;

        let el_genesis_json = l1_genesis.read_el_genesis()?;
        let jwt_secret_hex = l1_genesis.read_jwt_secret()?;

        let (l1_container_config, l2_container_config) =
            self.stable_config.as_ref().map_or((None, None), |config| {
                let l1_config = L1ContainerConfig {
                    use_stable_names: true,
                    network_name: Some(config.network_name.clone()),
                    http_port: Some(config.ports.l1_http),
                    engine_port: Some(config.ports.l1_auth),
                    beacon_http_port: Some(config.ports.l1_cl_http),
                    beacon_p2p_port: Some(config.ports.l1_cl_p2p),
                };
                let l2_config = L2ContainerConfig {
                    use_stable_names: true,
                    network_name: Some(config.network_name.clone()),
                    builder_http_port: Some(config.ports.l2_builder_http),
                    builder_ws_port: Some(config.ports.l2_builder_ws),
                    builder_auth_port: Some(config.ports.l2_builder_auth),
                    builder_p2p_port: Some(config.ports.l2_builder_p2p),
                    builder_flashblocks_port: Some(config.ports.l2_builder_flashblocks),
                    client_http_port: Some(config.ports.l2_client_http),
                    client_ws_port: Some(config.ports.l2_client_ws),
                    client_auth_port: Some(config.ports.l2_client_auth),
                    client_p2p_port: Some(config.ports.l2_client_p2p),
                    builder_consensus_rpc_port: Some(config.ports.l2_builder_cl_rpc),
                    builder_consensus_p2p_tcp_port: Some(config.ports.l2_builder_cl_p2p),
                    builder_consensus_p2p_udp_port: None,
                    client_consensus_rpc_port: Some(config.ports.l2_client_cl_rpc),
                    client_consensus_p2p_tcp_port: Some(config.ports.l2_client_cl_p2p),
                    client_consensus_p2p_udp_port: None,
                };
                (Some(l1_config), Some(l2_config))
            });

        let l1_config = L1StackConfig {
            el_genesis_json,
            jwt_secret_hex,
            testnet_dir: l1_genesis.testnet_dir(),
            container_config: l1_container_config,
        };

        let l1_stack = L1Stack::start(l1_config).await.wrap_err("Failed to start L1 stack")?;

        let l1_internal_rpc_url = l1_stack.reth().internal_rpc_url();
        let l2_deployment =
            tokio::task::spawn_blocking(move || setup.deploy_l2_contracts(&l1_internal_rpc_url))
                .await
                .wrap_err("L2 deployment task panicked")?
                .wrap_err("Failed to deploy L2 contracts")?;

        let jwt_secret = JwtSecret::random();

        let l2_genesis_bytes =
            std::fs::read(l2_deployment.genesis_path()).wrap_err("Failed to read L2 genesis")?;
        let rollup_config_bytes = std::fs::read(l2_deployment.rollup_config_path())
            .wrap_err("Failed to read rollup config")?;
        let l1_genesis_bytes =
            std::fs::read(l1_genesis.el_genesis_path()).wrap_err("Failed to read L1 genesis")?;

        let l2_config = L2StackConfig {
            l2_genesis: l2_genesis_bytes,
            rollup_config: rollup_config_bytes,
            l1_genesis: l1_genesis_bytes,
            jwt_secret,
            p2p_key: BUILDER.private_key,
            sequencer_key: SEQUENCER.private_key,
            batcher_key: BATCHER.private_key,
            l1_rpc_url: l1_stack.reth().rpc_url().await?.to_string(),
            l1_beacon_url: l1_stack.beacon().beacon_url().await?,
            container_config: l2_container_config,
            tx_forwarding_config: self.tx_forwarding_config,
            verifier_l1_confs: self.verifier_l1_confs,
            client_consensus_mode: self.client_consensus_mode,
        };

        let l2_stack = L2Stack::start(l2_config).await.wrap_err("Failed to start L2 stack")?;

        Ok(SystemTestStack { _temp_dir: temp_dir, l1_genesis, l2_deployment, l1_stack, l2_stack })
    }
}
