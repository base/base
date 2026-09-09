use core::{
    any::Any,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use std::{
    path::PathBuf,
    sync::{Arc, LazyLock},
};

use alloy_provider::{Identity, ProviderBuilder, RootProvider};
use async_trait::async_trait;
use base_common_network::Base;
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{BasePooledTransaction, TransactionPool};
use base_node_core::{BaseNode, NodeConfig, RollupArgs};
use base_node_runner::test_utils::init_silenced_tracing;
use futures::FutureExt;
use nanoid::nanoid;
use reth_node_core::{
    args::{DatadirArgs, NetworkArgs, RpcServerArgs},
    exit::NodeExitFuture,
};
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig};

use crate::{
    BuilderConfig, SharedMeteringStore,
    test_utils::{EngineApi, TransactionPoolObserver, create_test_db_env, driver::ChainDriver},
};

/// Clears OTEL-related environment variables that can interfere with CLI argument parsing.
/// This is necessary because clap reads env vars for args with `env = "..."` attributes,
/// and external OTEL env vars (e.g., `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`) may contain
/// values that are incompatible with the CLI's expected values.
pub fn clear_otel_env_vars() {
    for key in [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_LOGS_EXPORTER",
        "OTEL_METRICS_EXPORTER",
        "OTEL_TRACES_EXPORTER",
        "OTEL_SDK_DISABLED",
    ] {
        // SAFETY: We're in a test environment where env var mutation is acceptable
        unsafe { std::env::remove_var(key) };
    }
}

/// Represents a type that emulates a local in-process instance of the builder node.
/// Execution calls use the local driver; public transaction queries use HTTP.
#[derive(Debug)]
pub struct LocalInstance {
    /// In-process execution services.
    pub execution: base_execution_payload_builder::BaseExecutionHandle,
    /// Public HTTP endpoint used by transaction-query tests.
    pub http_url: String,
    node_config: NodeConfig,
    builder_config: BuilderConfig,
    runtime: Option<Runtime>,
    exit_future: NodeExitFuture,
    node_handle: Option<Box<dyn Any + Send>>,
    pool_handle: Option<Arc<dyn ExternalTransactionPool>>,
    pool_observer: TransactionPoolObserver,
    metering_provider: SharedMeteringStore,
    /// Temporary directory backing the node's database, removed on drop.
    db_dir: PathBuf,
}

struct PoolHandle<P> {
    pool: P,
}

impl<P: core::fmt::Debug> core::fmt::Debug for PoolHandle<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PoolHandle").field("pool", &self.pool).finish()
    }
}

/// Trait for submitting transactions to the pool from outside the node.
#[async_trait]
pub trait ExternalTransactionPool: Send + Sync + core::fmt::Debug {
    /// Submits a pooled transaction as if it arrived from an external peer.
    async fn add_external_transaction(&self, tx: BasePooledTransaction) -> eyre::Result<()>;
}

#[async_trait]
impl<P> ExternalTransactionPool for PoolHandle<P>
where
    P: TransactionPool + Send + Sync + core::fmt::Debug,
{
    async fn add_external_transaction(&self, tx: BasePooledTransaction) -> eyre::Result<()> {
        TransactionPool::add_external_transaction(&self.pool, tx)
            .await
            .map(|_| ())
            .map_err(|err| eyre::eyre!("pool rejected transaction: {err}"))
    }
}

/// Configures a [`LocalInstance`] using the production Base launch path.
///
/// ```ignore
/// let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
///     .with_builder_rpc(BuilderApiConfig::default())
///     .build()
///     .await?;
/// ```
#[derive(derive_more::Debug)]
pub struct LocalInstanceBuilder {
    builder_config: BuilderConfig,
    node_config: NodeConfig,
    rpc: base_node_core::BaseRpcServices,
}

impl LocalInstanceBuilder {
    /// Creates a new builder with the given builder configuration, the default node configuration,
    /// and the built-in sequencer RPC handlers.
    pub fn new(builder_config: BuilderConfig) -> Self {
        Self {
            builder_config,
            node_config: default_node_config(),
            rpc: base_node_core::BaseRpcServices {
                builder: Some(Default::default()),
                ..Default::default()
            },
        }
    }
}

impl LocalInstanceBuilder {
    /// Overrides the Reth node configuration.
    #[must_use]
    pub fn with_node_config(mut self, node_config: NodeConfig) -> Self {
        self.node_config = node_config;
        self
    }

    /// Sets the sequencer ingress limits used by the production RPC handler.
    pub fn with_builder_rpc(mut self, config: crate::BuilderApiConfig) -> Self {
        self.rpc.builder = Some(config);
        self
    }

    /// Launches the node described by this builder and returns the running [`LocalInstance`].
    ///
    /// This method does not prefund any accounts, so before sending any transactions make sure that
    /// sender accounts are funded.
    pub async fn build(self) -> eyre::Result<LocalInstance> {
        Box::pin(LocalInstance::launch(self.builder_config, self.node_config, self.rpc)).await
    }
}

impl LocalInstance {
    /// Creates a new local instance of the builder node with the given builder configuration,
    /// with the default Reth node configuration.
    ///
    /// This method does not prefund any accounts, so before sending any transactions
    /// make sure that sender accounts are funded.
    pub async fn new(builder_config: BuilderConfig) -> eyre::Result<Self> {
        Box::pin(LocalInstanceBuilder::new(builder_config).build()).await
    }

    /// Creates a new local instance of the builder node with the given builder configuration,
    /// with a given Reth node configuration.
    ///
    /// This method does not prefund any accounts, so before sending any transactions
    /// make sure that sender accounts are funded.
    pub async fn new_with_node_config(
        builder_config: BuilderConfig,
        node_config: NodeConfig,
    ) -> eyre::Result<Self> {
        Box::pin(LocalInstanceBuilder::new(builder_config).with_node_config(node_config).build())
            .await
    }

    /// Core launch routine shared by all constructors.
    ///
    /// Starts the production payload service and RPC handlers, then captures the pool handles.
    async fn launch(
        builder_config: BuilderConfig,
        node_config: NodeConfig,
        rpc: base_node_core::BaseRpcServices,
    ) -> eyre::Result<Self> {
        clear_otel_env_vars();
        init_silenced_tracing();
        let runtime = RuntimeBuilder::new(RuntimeConfig::default()).build()?;

        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();
        let metering_provider = Arc::clone(&builder_config.metering_provider);

        let base_node = BaseNode::new(RollupArgs::default())
            .with_da_config(da_config)
            .with_gas_limit_config(gas_limit_config);

        let service_builder = builder_config.clone().into_payload_service_config();

        let (db, db_dir) = create_test_db_env(node_config.clone())?;

        let mut builder = base_node_core::NodeLaunch::new(node_config.clone(), db, runtime.clone());
        builder.base = base_node;
        builder.payload = Some(service_builder);
        builder.rpc = rpc;

        let node_handle = builder.launch().await?;
        let pool_monitor = node_handle.node.pool.all_transactions_event_listener();
        let pool_handle: Arc<dyn ExternalTransactionPool> =
            Arc::new(PoolHandle { pool: node_handle.node.pool.clone() });
        let execution = node_handle.node.execution.clone();
        let http_url =
            node_handle.node.rpc_server_handle().http_url().expect("test HTTP RPC enabled");
        let exit_future = node_handle.node_exit_future;
        let node_handle: Box<dyn Any + Send> = Box::new(node_handle.node);

        Ok(Self {
            execution,
            http_url,
            builder_config,
            node_config,
            exit_future,
            node_handle: Some(node_handle),
            pool_handle: Some(pool_handle),
            runtime: Some(runtime),
            pool_observer: TransactionPoolObserver::new(pool_monitor),
            metering_provider,
            db_dir,
        })
    }

    /// Returns the Reth node configuration.
    pub const fn node_config(&self) -> &NodeConfig {
        &self.node_config
    }

    /// Returns the builder configuration.
    pub const fn builder_config(&self) -> &BuilderConfig {
        &self.builder_config
    }

    /// Accesses the node's execution driver and payload builder.
    pub fn engine_api(&self) -> EngineApi {
        EngineApi { execution: self.execution.clone() }
    }

    /// Returns a reference to the transaction pool observer.
    pub const fn pool(&self) -> &TransactionPoolObserver {
        &self.pool_observer
    }

    /// Returns a cloned handle for submitting external transactions to the pool.
    pub fn pool_handle(&self) -> Arc<dyn ExternalTransactionPool> {
        Arc::clone(self.pool_handle.as_ref().expect("pool handle present"))
    }

    /// Returns a reference to the shared metering provider.
    pub fn metering_provider(&self) -> &SharedMeteringStore {
        &self.metering_provider
    }

    /// Creates a [`ChainDriver`] connected to this local instance.
    pub async fn driver(&self) -> eyre::Result<ChainDriver> {
        ChainDriver::local(self).await
    }

    /// Creates an alloy provider for the public HTTP endpoint.
    pub async fn provider(&self) -> eyre::Result<RootProvider<Base>> {
        Ok(ProviderBuilder::<Identity, Identity, Base>::default()
            .connect_http(self.http_url.parse()?))
    }
}

impl Drop for LocalInstance {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // Tokio runtimes cannot perform their blocking shutdown while they are being dropped
            // from another runtime's async context. `LocalInstance` is commonly owned directly by
            // async tests, so shut down and drop its runtime on a plain thread before cleaning up
            // the resources it owns.
            let shutdown = std::thread::spawn(move || {
                runtime.graceful_shutdown_with_timeout(Duration::from_secs(10));
                drop(runtime);
            });
            if let Err(panic) = shutdown.join() {
                std::panic::resume_unwind(panic);
            }
            // Drop the node and the pool handle (both hold open database handles via the node's
            // provider / the pool's transaction validator) before removing the backing files.
            drop(self.node_handle.take());
            drop(self.pool_handle.take());
            if let Err(e) = std::fs::remove_dir_all(self.node_config().datadir().to_string()) {
                eprintln!(
                    "Warning: failed to remove temporary data directory {}: {e}",
                    self.node_config().datadir()
                );
            }
            if let Err(e) = std::fs::remove_dir_all(&self.db_dir) {
                eprintln!(
                    "Warning: failed to remove temporary database directory {}: {e}",
                    self.db_dir.display()
                );
            }
        }
    }
}

impl Future for LocalInstance {
    type Output = eyre::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().exit_future.poll_unpin(cx)
    }
}

/// Returns the default Reth node configuration used in tests.
pub fn default_node_config() -> NodeConfig {
    node_config_with_chain_spec(chain_spec())
}

/// Returns the default test chain spec, lazily initialized from the embedded
/// genesis template.
pub fn chain_spec() -> Arc<BaseChainSpec> {
    static CHAIN_SPEC: LazyLock<Arc<BaseChainSpec>> = LazyLock::new(|| {
        let genesis = include_str!("./artifacts/genesis.json.tmpl");
        let genesis = serde_json::from_str(genesis).expect("invalid genesis JSON");
        let chain_spec = BaseChainSpec::from_genesis(genesis);
        Arc::new(chain_spec)
    });

    CHAIN_SPEC.clone()
}

/// Returns a chain spec identical to the default test chain spec but with
/// `BaseUpgrade::Azul` activated at genesis (timestamp 0).
pub fn chain_spec_with_azul() -> Arc<BaseChainSpec> {
    use alloy_hardforks::ForkCondition;
    use base_common_evm::BaseUpgrade;

    let genesis = include_str!("./artifacts/genesis.json.tmpl");
    let genesis = serde_json::from_str(genesis).expect("invalid genesis JSON");
    let mut spec = BaseChainSpec::from_genesis(genesis);
    spec.config.upgrades.insert(BaseUpgrade::Azul, ForkCondition::Timestamp(0));
    Arc::new(spec)
}

/// Returns a node config using a chain spec with `BaseUpgrade::Azul` activated
/// at genesis.
pub fn default_node_config_with_azul() -> NodeConfig {
    node_config_with_chain_spec(chain_spec_with_azul())
}

/// Builds a [`LocalInstance`]-style Reth node configuration for the given chain spec.
///
/// Uses the same HTTP RPC setup, disabled discovery, unused ports, and temporary data
/// directories as [`default_node_config`], but with a caller-supplied chain spec — so an in-process
/// builder node can be launched against a custom genesis (e.g. one derived from a rollup config).
pub fn node_config_with_chain_spec(spec: Arc<BaseChainSpec>) -> NodeConfig {
    let tempdir = std::env::temp_dir();
    let random_id = nanoid!();

    let data_path = tempdir.join(format!("rbuilder.{random_id}.datadir"));
    let rocksdb_path = tempdir.join(format!("rbuilder.{random_id}.rocksdb"));
    let pprof_dumps_path = tempdir.join(format!("rbuilder.{random_id}.pprof-dumps"));

    std::fs::create_dir_all(&data_path).expect("Failed to create temporary data directory");
    std::fs::create_dir_all(&rocksdb_path).expect("Failed to create temporary rocksdb directory");
    std::fs::create_dir_all(&pprof_dumps_path)
        .expect("Failed to create temporary pprof dumps directory");

    let rpc = RpcServerArgs::default().with_unused_ports().with_http();

    let mut network = NetworkArgs::default().with_unused_ports();
    network.discovery.disable_discovery = true;

    let datadir = DatadirArgs {
        datadir: data_path.to_string_lossy().parse().expect("Failed to parse data dir path"),
        static_files_path: None,
        rocksdb_path: Some(rocksdb_path),
        pprof_dumps_path: Some(pprof_dumps_path),
    };

    NodeConfig::new(spec).with_datadir_args(datadir).with_rpc(rpc).with_network(network)
}
