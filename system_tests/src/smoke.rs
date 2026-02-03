use std::path::PathBuf;

use eyre::{Result, WrapErr};
use tempfile::TempDir;
use url::Url;

use crate::{
    config::{self, BATCHER, SEQUENCER},
    devnet_config::StableDevnetConfig,
    l1::{L1ContainerConfig, L1Stack, L1StackConfig},
    l2::{L2ContainerConfig, L2Stack, L2StackConfig},
    network::cleanup_network,
    setup::{BUILDER_P2P_KEY, L1GenesisOutput, L2DeploymentOutput, SetupContainer},
};

const DEFAULT_L1_CHAIN_ID: u64 = 1337;
const DEFAULT_L2_CHAIN_ID: u64 = 84538453;
const DEFAULT_SLOT_DURATION: u64 = 2;

/// A complete L1+L2 devnet stack.
pub struct Devnet {
    _temp_dir: TempDir,
    l1_genesis: L1GenesisOutput,
    l2_deployment: L2DeploymentOutput,
    l1_stack: L1Stack,
    l2_stack: Option<L2Stack>,
}

impl std::fmt::Debug for Devnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Devnet")
            .field("l1_genesis", &self.l1_genesis)
            .field("l2_deployment", &self.l2_deployment)
            .finish_non_exhaustive()
    }
}

impl Devnet {
    /// Returns a reference to the L1 stack.
    pub const fn l1_stack(&self) -> &L1Stack {
        &self.l1_stack
    }

    /// Returns a reference to the L2 stack.
    #[allow(clippy::missing_const_for_fn)]
    pub fn l2_stack(&self) -> &L2Stack {
        self.l2_stack.as_ref().expect("L2Stack already dropped")
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
}

impl Drop for Devnet {
    fn drop(&mut self) {
        drop(self.l2_stack.take());
        cleanup_network();
    }
}

/// Builder for creating a new `Devnet`.
#[derive(Debug, Default)]
pub struct DevnetBuilder {
    l1_chain_id: Option<u64>,
    l2_chain_id: Option<u64>,
    slot_duration: Option<u64>,
    output_dir: Option<PathBuf>,
    stable_config: Option<StableDevnetConfig>,
}

impl DevnetBuilder {
    /// Creates a new `DevnetBuilder` with default configuration.
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

    /// Sets the output directory for devnet files.
    pub fn with_output_dir(mut self, output_dir: PathBuf) -> Self {
        self.output_dir = Some(output_dir);
        self
    }

    /// Enables stable container names and ports matching docker-compose.yml.
    pub fn with_stable_config(mut self) -> Self {
        self.stable_config = Some(StableDevnetConfig::devnet());
        self
    }

    /// Builds and starts the devnet.
    pub async fn build(self) -> Result<Devnet> {
        let l1_chain_id = self.l1_chain_id.unwrap_or(DEFAULT_L1_CHAIN_ID);
        let l2_chain_id = self.l2_chain_id.unwrap_or(DEFAULT_L2_CHAIN_ID);
        let slot_duration = self.slot_duration.unwrap_or(DEFAULT_SLOT_DURATION);

        let temp_dir = TempDir::new().wrap_err("Failed to create temp directory")?;
        let output_dir = self.output_dir.unwrap_or_else(|| temp_dir.path().to_path_buf());

        let mut setup = SetupContainer::new(&output_dir)
            .with_chain_id(l1_chain_id)
            .with_l2_chain_id(l2_chain_id)
            .with_slot_duration(slot_duration);

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
                    op_node_rpc_port: Some(config.ports.l2_builder_cl_rpc),
                    op_node_p2p_port: Some(config.ports.l2_builder_cl_p2p),
                    op_node_follower_rpc_port: Some(config.ports.l2_client_cl_rpc),
                    batcher_metrics_port: Some(config.ports.batcher_metrics),
                    builder_http_port: Some(config.ports.l2_builder_http),
                    builder_ws_port: Some(config.ports.l2_builder_ws),
                    builder_auth_port: Some(config.ports.l2_builder_auth),
                    builder_p2p_port: Some(config.ports.l2_builder_p2p),
                    builder_flashblocks_port: Some(config.ports.l2_builder_flashblocks),
                    client_http_port: Some(config.ports.l2_client_http),
                    client_ws_port: Some(config.ports.l2_client_ws),
                    client_auth_port: Some(config.ports.l2_client_auth),
                    client_p2p_port: Some(config.ports.l2_client_p2p),
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

        let jwt_secret = config::random_jwt_secret_hex();

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
            jwt_secret_hex: jwt_secret.into_bytes(),
            p2p_key: BUILDER_P2P_KEY.as_bytes().to_vec(),
            sequencer_key: format!("0x{}", hex::encode(SEQUENCER.private_key)).into_bytes(),
            batcher_key: format!("0x{}", hex::encode(BATCHER.private_key)),
            l1_rpc_url: l1_stack.reth().internal_rpc_url(),
            l1_beacon_url: l1_stack.beacon().internal_beacon_url(),
            container_config: l2_container_config,
        };

        let l2_stack = L2Stack::start(l2_config).await.wrap_err("Failed to start L2 stack")?;

        Ok(Devnet { _temp_dir: temp_dir, l1_genesis, l2_deployment, l1_stack, l2_stack: Some(l2_stack) })
    }
}
