//! System test stack orchestration and lifecycle management.

#[cfg(feature = "upgrade-signal")]
use std::{
    collections::BTreeSet,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use std::{num::NonZeroU64, path::PathBuf};

use alloy_network::Ethereum;
use alloy_primitives::B256;
use alloy_provider::RootProvider;
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_engine::JwtSecret;
use alloy_signer_local::PrivateKeySigner;
#[cfg(feature = "upgrade-signal")]
use base_common_genesis::{BaseUpgrade, RollupConfig, RuntimeUpgradeRegistry, UpgradeActivation};
use base_common_network::Base;
use base_node_runner::BaseNodeExtension;
use base_tx_forwarding::TxForwardingConfig;
#[cfg(feature = "upgrade-signal")]
use eyre::ensure;
use eyre::{OptionExt, Result, WrapErr};
use tempfile::TempDir;
use url::Url;

#[cfg(feature = "upgrade-signal")]
use crate::upgrade_signal::{MockProtocolVersionsClient, UpgradeSignalStackOptions};
use crate::{
    BATCHER, BUILDER, SEQUENCER,
    l1::{L1ContainerConfig, L1Execution, L1RpcProxy, L1Stack, L1StackConfig},
    l2::{
        L2ClientConsensusMode, L2ContainerConfig, L2Stack, L2StackConfig, ShadowSequencersConfig,
    },
    setup::{L1GenesisOutput, L2DeploymentOutput, SetupContainer},
    system_config::StableSystemTestConfig,
    telemetry::{TelemetryIngest, TelemetryStackOptions},
};

const DEFAULT_L1_CHAIN_ID: u64 = 1337;
const DEFAULT_L2_CHAIN_ID: u64 = 84538453;
/// L1 beacon slot duration. Live `op-deployer` confirms one transaction per L1
/// slot, so this dominates stack startup time.
const DEFAULT_SLOT_DURATION: u64 = 1;
const DEFAULT_SHADOW_BLOCKS_PER_CYCLE: NonZeroU64 = NonZeroU64::new(3).unwrap();
/// Prefix for the network name reported by a telemetry-enabled stack.
///
/// A system test chain has no registered network name, so reports are labelled by chain ID
/// behind a prefix that can never be mistaken for a real network.
const TELEMETRY_NETWORK_PREFIX: &str = "system-test-";

/// Longest wait for a live L1 schedule change to be re-applied by a runtime-admin node (the
/// upgrade signal poll interval is 12s).
#[cfg(feature = "upgrade-signal")]
const LIVE_APPLY_TIMEOUT: Duration = Duration::from_secs(90);
/// Interval between runtime registry checks while awaiting a live schedule apply.
#[cfg(feature = "upgrade-signal")]
const LIVE_APPLY_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[cfg(feature = "upgrade-signal")]
static RUNTIME_UPGRADE_SIGNAL_OWNERS: OnceLock<Mutex<BTreeSet<u64>>> = OnceLock::new();

/// Exclusive ownership of the process-global [`RuntimeUpgradeRegistry`] entries for one L2
/// chain ID, cleared when dropped.
///
/// A runtime-admin [`SystemTestStack`] acquires this guard before its nodes start so that its
/// live schedule overrides never outlive the stack and never contaminate a later stack reusing
/// the same chain ID. Ownership is exclusive: acquiring a chain ID that another live guard
/// already owns fails, because the registry is shared by every in-process node in the test
/// binary and two concurrent writers for the same chain cannot be isolated from each other.
#[cfg(feature = "upgrade-signal")]
#[derive(Debug)]
pub struct RuntimeUpgradeSignalGuard {
    chain_id: u64,
}

#[cfg(feature = "upgrade-signal")]
impl RuntimeUpgradeSignalGuard {
    /// Acquires exclusive runtime-registry ownership of the given L2 chain ID.
    ///
    /// # Errors
    ///
    /// Returns an error if another live guard already owns the chain ID; use a unique L2 chain
    /// ID per concurrently running runtime-admin stack.
    pub fn acquire(chain_id: u64) -> Result<Self> {
        let owners = RUNTIME_UPGRADE_SIGNAL_OWNERS.get_or_init(|| Mutex::new(BTreeSet::new()));
        ensure!(
            owners.lock().expect("runtime upgrade signal owner lock poisoned").insert(chain_id),
            "another running runtime-admin SystemTestStack already owns runtime upgrade \
             overrides for L2 chain ID {chain_id}; use a unique L2 chain ID per stack"
        );
        Ok(Self { chain_id })
    }
}

#[cfg(feature = "upgrade-signal")]
impl Drop for RuntimeUpgradeSignalGuard {
    fn drop(&mut self) {
        RuntimeUpgradeRegistry::clear_chain(self.chain_id);
        RUNTIME_UPGRADE_SIGNAL_OWNERS
            .get_or_init(|| Mutex::new(BTreeSet::new()))
            .lock()
            .expect("runtime upgrade signal owner lock poisoned")
            .remove(&self.chain_id);
    }
}

/// A complete L1+L2 stack for system tests.
pub struct SystemTestStack {
    _temp_dir: TempDir,
    #[cfg(feature = "upgrade-signal")]
    l2_chain_id: u64,
    l1_genesis: L1GenesisOutput,
    l2_deployment: L2DeploymentOutput,
    l1_stack: L1Stack,
    l2_stack: L2Stack,
    l1_rpc_proxy: Option<L1RpcProxy>,
    telemetry: Option<TelemetryIngest>,
    #[cfg(feature = "upgrade-signal")]
    upgrade_signal: Option<MockProtocolVersionsClient>,
    /// Must be the last field: it clears the runtime registry on drop, so the stacks above
    /// (and their upgrade-signal writer tasks) must shut down first.
    #[cfg(feature = "upgrade-signal")]
    _runtime_upgrade_signal_guard: Option<RuntimeUpgradeSignalGuard>,
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

    /// Returns the controllable L1 RPC proxy when fault injection was enabled.
    pub const fn l1_rpc_proxy(&self) -> Option<&L1RpcProxy> {
        self.l1_rpc_proxy.as_ref()
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

    /// Returns the number of shadow sequencers running in this stack.
    pub fn shadow_sequencer_count(&self) -> usize {
        self.l2_stack().shadow_sequencers().len()
    }

    /// Returns a builder provider for the shadow sequencer at `index`.
    pub fn l2_shadow_builder_provider(&self, index: usize) -> Result<RootProvider<Base>> {
        let shadow = self
            .l2_stack()
            .shadow_sequencer(index)
            .ok_or_eyre("no shadow sequencer at the requested index")?;
        let url = shadow.rpc_url()?;
        let client = RpcClient::builder().http(url);
        Ok(RootProvider::<Base>::new(client))
    }

    /// Returns the telemetry ingest endpoint, when the stack was built with
    /// [`SystemTestStackBuilder::with_telemetry`].
    ///
    /// # Panics
    ///
    /// Panics if the stack was built without telemetry.
    pub const fn telemetry(&self) -> &TelemetryIngest {
        self.telemetry.as_ref().expect("stack was not built with telemetry enabled")
    }

    /// Returns the mock L1 upgrade signal contract client, when the stack was built with
    /// [`SystemTestStackBuilder::with_upgrade_signal`].
    #[cfg(feature = "upgrade-signal")]
    pub const fn upgrade_signal(&self) -> Option<&MockProtocolVersionsClient> {
        self.upgrade_signal.as_ref()
    }

    /// Writes an upgrade schedule on L1 and waits until the process-local
    /// [`RuntimeUpgradeRegistry`] reflects the activation timestamp for this stack's L2 chain.
    ///
    /// Requires the stack to have been built with
    /// [`SystemTestStackBuilder::with_upgrade_signal`] and a node running in a runtime-admin
    /// mode that re-applies live schedule changes.
    #[cfg(feature = "upgrade-signal")]
    pub async fn set_schedule_and_await_runtime_apply(
        &self,
        upgrade: BaseUpgrade,
        activation_timestamp: u64,
    ) -> Result<()> {
        let contract = self
            .upgrade_signal
            .as_ref()
            .ok_or_eyre("stack was not built with the upgrade signal enabled")?;
        contract.set_schedule(&[(upgrade, activation_timestamp)]).await.wrap_err_with(|| {
            format!("Failed to update {} schedule on L1", upgrade.contract_id())
        })?;

        tokio::time::timeout(LIVE_APPLY_TIMEOUT, async {
            loop {
                if RuntimeUpgradeRegistry::activation(self.l2_chain_id, upgrade)
                    == Some(UpgradeActivation::Timestamp(activation_timestamp))
                {
                    return;
                }
                tokio::time::sleep(LIVE_APPLY_POLL_INTERVAL).await;
            }
        })
        .await
        .map_err(|_| {
            eyre::eyre!(
                "live L1 {} schedule was not re-applied to the runtime registry within {}s",
                upgrade.contract_id(),
                LIVE_APPLY_TIMEOUT.as_secs()
            )
        })
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
    base_cobalt_activation_block: Option<u64>,
    base_denim_activation_block: Option<u64>,
    base_zenith_activation_block: Option<u64>,
    output_dir: Option<PathBuf>,
    stable_config: Option<StableSystemTestConfig>,
    tx_forwarding_config: Option<TxForwardingConfig>,
    enable_experimental_validity_transactions: bool,
    verifier_l1_confs: u64,
    client_consensus_mode: L2ClientConsensusMode,
    shadow_sequencer_count: usize,
    shadow_blocks_per_cycle: Option<NonZeroU64>,
    shadow_start_block: Option<u64>,
    tmpfs_datadirs: bool,
    l1_fault_injection: bool,
    extra_builder_extensions: Vec<Box<dyn BaseNodeExtension>>,
    extra_client_extensions: Vec<Box<dyn BaseNodeExtension>>,
    telemetry: Option<TelemetryStackOptions>,
    #[cfg(feature = "upgrade-signal")]
    upgrade_signal: Option<UpgradeSignalStackOptions>,
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

    /// Sets the L1 beacon slot duration in seconds.
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

    /// Sets the L2 block number at which Base Cobalt activates.
    pub const fn with_base_cobalt_activation_block(mut self, block: u64) -> Self {
        self.base_cobalt_activation_block = Some(block);
        self
    }

    /// Sets the L2 block number at which Base Denim activates.
    pub const fn with_base_denim_activation_block(mut self, block: u64) -> Self {
        self.base_denim_activation_block = Some(block);
        self
    }

    /// Sets the L2 block number at which the genesis-only Base Zenith testing gate activates.
    pub const fn with_base_zenith_activation_block(mut self, block: u64) -> Self {
        self.base_zenith_activation_block = Some(block);
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

    /// Enables experimental validity transaction ingress and builder acceptance.
    pub const fn with_experimental_validity_transactions(mut self) -> Self {
        self.enable_experimental_validity_transactions = true;
        self
    }

    /// Backs the L1 container datadirs (reth, lighthouse) with tmpfs.
    ///
    /// reth's mdbx and lighthouse's database need a writable `MAP_SHARED` mmap, which some
    /// container storage backends reject — notably overlayfs on docker-in-docker CI runners, where
    /// reth exits at startup with "failed to open the database: Remote I/O error (121)". tmpfs
    /// supports mmap. Enable this to run the stack on such runners; it is a no-op where the storage
    /// already supports mmap.
    pub const fn with_tmpfs_datadirs(mut self) -> Self {
        self.tmpfs_datadirs = true;
        self
    }

    /// Routes L2 services through a controllable L1 RPC proxy and enables L1 reorg control.
    pub const fn with_l1_fault_injection(mut self) -> Self {
        self.l1_fault_injection = true;
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

    /// Runs `count` shadow sequencers alongside the active sequencer.
    ///
    /// Each shadow sequencer builds real blocks from its own mempool but signs
    /// them with a distinct key, so its blocks are non-canonical to the rest of
    /// the network.
    pub const fn with_shadow_sequencers(mut self, count: usize) -> Self {
        self.shadow_sequencer_count = count;
        self
    }

    /// Sets the number of private blocks each shadow sequencer builds per
    /// reconciliation cycle. Defaults to [`DEFAULT_SHADOW_BLOCKS_PER_CYCLE`].
    pub const fn with_shadow_blocks_per_cycle(mut self, blocks: NonZeroU64) -> Self {
        self.shadow_blocks_per_cycle = Some(blocks);
        self
    }

    /// Delays starting configured shadow sequencers until the active builder reaches `block`.
    pub const fn with_shadow_start_block(mut self, block: u64) -> Self {
        self.shadow_start_block = Some(block);
        self
    }

    /// Registers an additional node extension on the L2 builder, installed after its built-in
    /// RPC wiring.
    ///
    /// Lets downstream consumers layer their own [`BaseNodeExtension`] onto the standard builder
    /// wiring without forking this crate.
    pub fn with_builder_extension(mut self, extension: Box<dyn BaseNodeExtension>) -> Self {
        self.extra_builder_extensions.push(extension);
        self
    }

    /// Registers an additional node extension on the L2 client, installed after its built-in
    /// extensions.
    ///
    /// Lets downstream consumers layer their own [`BaseNodeExtension`] — such as a custom RPC
    /// method — onto the standard client wiring without forking this crate.
    pub fn with_client_extension(mut self, extension: Box<dyn BaseNodeExtension>) -> Self {
        self.extra_client_extensions.push(extension);
        self
    }

    /// Enables node telemetry: starts an ingest endpoint on an ephemeral loopback port and
    /// starts the client consensus node reporting to it.
    pub const fn with_telemetry(mut self, options: TelemetryStackOptions) -> Self {
        self.telemetry = Some(options);
        self
    }

    /// Enables the L1 upgrade signal: deploys a mock `ProtocolVersions` contract to L1, seeds
    /// it with the options' schedule, and starts both consensus nodes (and, when an execution
    /// mode is set, the client execution node) reading it.
    #[cfg(feature = "upgrade-signal")]
    pub fn with_upgrade_signal(mut self, options: UpgradeSignalStackOptions) -> Self {
        self.upgrade_signal = Some(options);
        self
    }

    /// Builds and starts the system test stack.
    pub async fn build(self) -> Result<SystemTestStack> {
        let l1_chain_id = self.l1_chain_id.unwrap_or(DEFAULT_L1_CHAIN_ID);
        let l2_chain_id = self.l2_chain_id.unwrap_or(DEFAULT_L2_CHAIN_ID);
        let slot_duration = self.slot_duration.unwrap_or(DEFAULT_SLOT_DURATION);

        // Acquire runtime-registry ownership before any node starts, so live overrides are
        // cleared even when a later startup step fails, and so a chain-ID conflict with a
        // concurrently running runtime-admin stack fails fast.
        #[cfg(feature = "upgrade-signal")]
        let runtime_upgrade_signal_guard = self
            .upgrade_signal
            .as_ref()
            .filter(|options| {
                options.mode.allows_runtime_admin()
                    || options.execution_mode.is_some_and(|mode| mode.allows_runtime_admin())
            })
            .map(|_| RuntimeUpgradeSignalGuard::acquire(l2_chain_id))
            .transpose()?;

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

        if let Some(block) = self.base_cobalt_activation_block {
            setup = setup.with_base_cobalt_activation_block(block);
        }

        if let Some(block) = self.base_denim_activation_block {
            setup = setup.with_base_denim_activation_block(block);
        }

        if let Some(block) = self.base_zenith_activation_block {
            setup = setup.with_base_zenith_activation_block(block);
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
                    tmpfs_datadir: self.tmpfs_datadirs,
                    enable_reorg_control: self.l1_fault_injection,
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

        // Ensure the tmpfs-datadir request reaches the L1 containers even without a stable config.
        let l1_container_config = l1_container_config.or_else(|| {
            (self.tmpfs_datadirs || self.l1_fault_injection).then(|| L1ContainerConfig {
                tmpfs_datadir: self.tmpfs_datadirs,
                enable_reorg_control: self.l1_fault_injection,
                ..Default::default()
            })
        });

        let l1_config = L1StackConfig {
            el_genesis_json,
            jwt_secret_hex,
            testnet_dir: l1_genesis.testnet_dir(),
            container_config: l1_container_config,
        };

        // Start Reth first, then overlap live L2 deployment with Lighthouse
        // startup. `op-deployer apply` only needs the EL RPC; its transactions
        // sit in the mempool until the validator begins producing blocks.
        let l1_execution =
            L1Execution::start(l1_config).await.wrap_err("Failed to start L1 execution layer")?;
        let l1_internal_rpc_url = l1_execution.reth().internal_rpc_url();
        let deploy_handle =
            tokio::task::spawn_blocking(move || setup.deploy_l2_contracts(&l1_internal_rpc_url));
        let l1_stack =
            l1_execution.start_consensus().await.wrap_err("Failed to start L1 consensus")?;
        let l2_deployment = deploy_handle
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

        let shadow_sequencers = (self.shadow_sequencer_count > 0).then(|| {
            let keys = (0..self.shadow_sequencer_count)
                .map(|_| {
                    B256::from_slice(PrivateKeySigner::random().credential().to_bytes().as_slice())
                })
                .collect();
            ShadowSequencersConfig {
                keys,
                blocks_per_cycle: self
                    .shadow_blocks_per_cycle
                    .unwrap_or(DEFAULT_SHADOW_BLOCKS_PER_CYCLE),
                start_block: self.shadow_start_block,
            }
        });

        // The upgrade-signal path deploys a mock ProtocolVersions contract via base-test-utils.
        // That crate embeds Foundry artifacts at compile time and cannot build as a git
        // dependency, so it and this path are gated behind the (default-on) `upgrade-signal`
        // feature. With the feature off the stack simply runs without an L1 upgrade signal.
        #[cfg(feature = "upgrade-signal")]
        let (upgrade_signal, l2_upgrade_signal, l2_execution_upgrade_signal) = {
            let upgrade_signal = match &self.upgrade_signal {
                Some(options) => {
                    let rollup_config: RollupConfig = serde_json::from_slice(&rollup_config_bytes)
                        .wrap_err("Failed to parse rollup config for upgrade signal baseline")?;
                    let l1_public_rpc_url = l1_stack.rpc_url().await?;
                    let client = MockProtocolVersionsClient::deploy(
                        l1_public_rpc_url,
                        options,
                        &rollup_config,
                    )
                    .await
                    .wrap_err("Failed to deploy upgrade signal mock contract")?;
                    Some(client)
                }
                None => None,
            };
            let signal_parts = self.upgrade_signal.as_ref().zip(upgrade_signal.as_ref());
            let l2_upgrade_signal =
                signal_parts.map(|(options, client)| options.signal_config(client.address));
            let l2_execution_upgrade_signal = signal_parts.and_then(|(options, client)| {
                options.execution_signal_config(client.address, client.l1_rpc_url.clone())
            });
            (upgrade_signal, l2_upgrade_signal, l2_execution_upgrade_signal)
        };
        #[cfg(not(feature = "upgrade-signal"))]
        let (l2_upgrade_signal, l2_execution_upgrade_signal) = (None, None);

        let direct_l1_rpc_url = l1_stack.reth().rpc_url().await?;
        let l1_rpc_proxy = if self.l1_fault_injection {
            Some(L1RpcProxy::start(direct_l1_rpc_url.clone()).await?)
        } else {
            None
        };
        let l2_l1_rpc_url = l1_rpc_proxy
            .as_ref()
            .map_or_else(|| direct_l1_rpc_url.to_string(), |proxy| proxy.url().to_string());

        // Started before the L2 stack so the node has an endpoint to report to from its first
        // reporting cycle, rather than failing the first one against a port nothing is on yet.
        let telemetry = match self.telemetry {
            Some(options) => Some(
                options.start().await.wrap_err("Failed to start the telemetry ingest endpoint")?,
            ),
            None => None,
        };

        let l2_config = L2StackConfig {
            l2_genesis: l2_genesis_bytes,
            rollup_config: rollup_config_bytes,
            l1_genesis: l1_genesis_bytes,
            jwt_secret,
            p2p_key: BUILDER.private_key,
            sequencer_key: SEQUENCER.private_key,
            batcher_key: BATCHER.private_key,
            l1_rpc_url: l2_l1_rpc_url,
            l1_beacon_url: l1_stack.beacon().beacon_url().await?,
            l1_slot_duration: slot_duration,
            container_config: l2_container_config,
            tx_forwarding_config: self.tx_forwarding_config,
            enable_experimental_validity_transactions: self
                .enable_experimental_validity_transactions,
            verifier_l1_confs: self.verifier_l1_confs,
            client_consensus_mode: self.client_consensus_mode,
            upgrade_signal: l2_upgrade_signal,
            execution_upgrade_signal: l2_execution_upgrade_signal,
            telemetry: telemetry.as_ref().map(|ingest| {
                ingest.node_config(
                    env!("CARGO_PKG_VERSION").to_string(),
                    format!("{TELEMETRY_NETWORK_PREFIX}{l2_chain_id}"),
                )
            }),
            shadow_sequencers,
            extra_builder_extensions: self.extra_builder_extensions,
            extra_client_extensions: self.extra_client_extensions,
        };

        let l2_stack = L2Stack::start(l2_config).await.wrap_err("Failed to start L2 stack")?;

        Ok(SystemTestStack {
            _temp_dir: temp_dir,
            #[cfg(feature = "upgrade-signal")]
            l2_chain_id,
            l1_genesis,
            l2_deployment,
            l1_stack,
            l2_stack,
            l1_rpc_proxy,
            telemetry,
            #[cfg(feature = "upgrade-signal")]
            upgrade_signal,
            #[cfg(feature = "upgrade-signal")]
            _runtime_upgrade_signal_guard: runtime_upgrade_signal_guard,
        })
    }
}
