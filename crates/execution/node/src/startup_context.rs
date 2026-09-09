//! Context for constructing the fixed Base node services.

use std::sync::Arc;

use alloy_eips::eip4844::env_settings::EnvKzgSettings;
use base_common_chain_config::BaseChainSpec;
use base_common_runtime_tasks::TaskExecutor;
use base_execution_txpool::{PoolConfig, TransactionPool};
use reth_network::{
    NetworkBuilder, NetworkConfig, NetworkConfigBuilder, NetworkHandle, NetworkManager,
    transactions::config::StrictEthAnnouncementFilter,
};
use reth_node_core::{
    dirs::{ChainPath, DataDirPath},
    node_config::NodeConfig,
    primitives::Head,
};
use base_execution_state_provider::{ChainSpecProvider, providers::BlockchainProvider};
use secp256k1::SecretKey;
use tracing::{info, trace, warn};

use crate::WithConfigs;

/// Captures the necessary context for building the components of the node.
pub struct BuilderContext {
    /// The current head of the blockchain at launch.
    pub(crate) head: Head,
    /// The configured provider to interact with the blockchain.
    pub(crate) provider: BlockchainProvider,
    /// The executor of the node.
    pub(crate) executor: TaskExecutor,
    /// Config container
    pub(crate) config_container: WithConfigs,
    /// Cache of recovered transaction senders shared by node components, if enabled.
    sender_recovery_cache: Option<base_execution_evm_blocks::SenderRecoveryCache>,
}

impl BuilderContext {
    /// Create a new instance of [`BuilderContext`]
    pub fn new(
        head: Head,
        provider: BlockchainProvider,
        executor: TaskExecutor,
        config_container: WithConfigs,
    ) -> Self {
        let sender_recovery_cache = config_container
            .config
            .engine
            .sender_recovery_cache_enabled
            .then(base_execution_evm_blocks::SenderRecoveryCache::default);
        Self { head, provider, executor, config_container, sender_recovery_cache }
    }

    /// Returns the configured provider to interact with the blockchain.
    pub const fn provider(&self) -> &BlockchainProvider {
        &self.provider
    }

    /// Returns the current head of the blockchain at launch.
    pub const fn head(&self) -> Head {
        self.head
    }

    /// Returns the config of the node.
    pub const fn config(&self) -> &NodeConfig {
        &self.config_container.config
    }

    /// Returns a mutable reference to the config of the node.
    pub const fn config_mut(&mut self) -> &mut NodeConfig {
        &mut self.config_container.config
    }

    /// Returns the loaded reh.toml config.
    pub const fn reth_config(&self) -> &reth_config::Config {
        &self.config_container.toml_config
    }

    /// Returns the executor of the node.
    ///
    /// This can be used to execute async tasks or functions during the setup.
    pub const fn task_executor(&self) -> &TaskExecutor {
        &self.executor
    }

    /// Returns the sender recovery cache shared by node components, if enabled.
    pub const fn sender_recovery_cache(
        &self,
    ) -> Option<&base_execution_evm_blocks::SenderRecoveryCache> {
        self.sender_recovery_cache.as_ref()
    }

    /// Returns the chain spec of the node.
    pub fn chain_spec(&self) -> Arc<BaseChainSpec> {
        self.provider().chain_spec()
    }

    /// Returns true if the node is configured as --dev
    pub const fn is_dev(&self) -> bool {
        self.config().dev.dev
    }

    /// Returns the transaction pool config of the node.
    pub fn pool_config(&self) -> PoolConfig {
        self.config().txpool.pool_config()
    }

    /// Loads `EnvKzgSettings::Default`.
    pub const fn kzg_settings(&self) -> eyre::Result<EnvKzgSettings> {
        Ok(EnvKzgSettings::Default)
    }

    /// Starts the Base network tasks using the configured propagation settings.
    pub fn start_network(
        &self,
        builder: NetworkBuilder<(), ()>,
        pool: base_node_context::BaseNodePool<BlockchainProvider>,
    ) -> NetworkHandle {
        let (handle, network, txpool, eth) = builder
            .transactions_with_policies(
                pool.clone(),
                self.config().network.transactions_manager_config(),
                self.config().network.tx_propagation_policy,
                StrictEthAnnouncementFilter::default(),
            )
            .map_transactions(|transactions| {
                if let Some(cache) = self.sender_recovery_cache.clone() {
                    transactions.with_sender_recovery_cache(cache)
                } else {
                    transactions
                }
            })
            .request_handler_with_blob_store(self.provider().clone(), pool.blob_store())
            .split_with_handle();

        self.executor.spawn_critical_blocking_task("p2p txpool", txpool);
        self.executor.spawn_critical_blocking_task("p2p eth request handler", eth);

        let default_peers_path = self.config().datadir().known_peers();
        let known_peers_file = self.config().network.persistent_peers_file(default_peers_path);
        self.executor.spawn_critical_with_graceful_shutdown_signal(
            "p2p network task",
            |shutdown| {
                network.run_until_graceful_shutdown(shutdown, |network| {
                    if let Some(peers_file) = known_peers_file {
                        let num_known_peers = network.num_known_peers();
                        trace!(target: "reth::cli", peers_file=?peers_file, num_peers=%num_known_peers, "Saving current peers");
                        match network.write_peers_to_file(peers_file.as_path()) {
                            Ok(_) => {
                                info!(target: "reth::cli", peers_file=?peers_file, "Wrote network peers to file");
                            }
                            Err(err) => {
                                warn!(target: "reth::cli", %err, "Failed to write network peers to file");
                            }
                        }
                    }
                })
            },
        );

        handle
    }

    /// Get the network secret from the given data dir
    fn network_secret(&self, data_dir: &ChainPath<DataDirPath>) -> eyre::Result<SecretKey> {
        let secret_key = self.config().network.secret_key(data_dir.p2p_secret())?;
        Ok(secret_key)
    }

    /// Builds the [`NetworkConfig`].
    pub fn build_network_config(
        &self,
        network_builder: NetworkConfigBuilder,
    ) -> NetworkConfig<BlockchainProvider> {
        network_builder.build(self.provider.clone())
    }
}

impl BuilderContext {
    /// Creates the [`NetworkBuilder`] for the node.
    pub async fn network_builder(&self) -> eyre::Result<NetworkBuilder<(), ()>> {
        let network_config = self.network_config()?;
        let builder = NetworkManager::builder(network_config).await?;
        Ok(builder)
    }

    /// Returns the default network config for the node.
    pub fn network_config(&self) -> eyre::Result<NetworkConfig<BlockchainProvider>> {
        let network_builder = self.network_config_builder();
        Ok(self.build_network_config(network_builder?))
    }

    /// Get the [`NetworkConfigBuilder`].
    pub fn network_config_builder(&self) -> eyre::Result<NetworkConfigBuilder> {
        let secret_key = self.network_secret(&self.config().datadir())?;
        let default_peers_path = self.config().datadir().known_peers();
        let builder = self
            .config()
            .network
            .network_config(
                self.reth_config(),
                &self.config().chain,
                secret_key,
                default_peers_path,
                self.executor.clone(),
            )
            .set_head(self.head);

        Ok(builder)
    }
}

impl std::fmt::Debug for BuilderContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuilderContext")
            .field("head", &self.head)
            .field("provider", &std::any::type_name::<BlockchainProvider>())
            .field("executor", &self.executor)
            .field("config", &self.config())
            .finish()
    }
}
