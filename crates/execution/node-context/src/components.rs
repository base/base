//! Traits for configuring a node.

use std::fmt::Debug;

use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::PayloadBuilderHandle;
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_provider::{FullProvider, providers::BlockchainProvider};
use reth_tasks::TaskExecutor;

/// Base's transaction pool with its production disk blob store.
pub type BaseNodePool<Provider> =
    base_execution_txpool::BaseTransactionPool<Provider, base_execution_txpool::DiskFileBlobStore>;

/// Encapsulates all types and components of the node.
pub trait FullNodeComponents: Clone + Debug + Send + Sync + Unpin + 'static {
    /// Underlying database used by the node.
    type DB: Database + DatabaseMetrics + Clone + Unpin + 'static;
    /// State access interface exposed by the node.
    type Provider: FullProvider<Self::DB>;

    /// Returns the transaction pool of the node.
    fn pool(&self) -> &BaseNodePool<Self::Provider>;

    /// Returns the node's evm config.
    fn evm_config(&self) -> &BaseEvmConfig;

    /// Returns the node's consensus type.
    fn consensus(&self) -> &std::sync::Arc<base_execution_consensus::BaseBeaconConsensus>;

    /// Returns the handle to the network
    fn network(&self) -> &reth_network::NetworkHandle;

    /// Returns the handle to the payload builder service handling payload building requests from
    /// the engine.
    fn payload_builder_handle(&self) -> &PayloadBuilderHandle;

    /// Returns the provider of the node.
    fn provider(&self) -> &Self::Provider;

    /// Returns an executor handle to spawn tasks.
    ///
    /// This can be used to spawn critical, blocking tasks or register tasks that should be
    /// terminated gracefully.
    fn task_executor(&self) -> &TaskExecutor;
}

/// Container for the node's types and the components and other internals that can be used by
/// addons of the node.
#[derive(Debug, Clone)]
pub struct BaseNodeContext<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> {
    /// The node transaction pool.
    pub transaction_pool: BaseNodePool<BlockchainProvider<DB>>,
    /// The Base EVM configuration.
    pub evm_config: BaseEvmConfig,
    /// The Base consensus validator.
    pub consensus: std::sync::Arc<base_execution_consensus::BaseBeaconConsensus>,
    /// The network handle.
    pub network: reth_network::NetworkHandle,
    /// The payload service handle.
    pub payload_builder_handle: base_execution_payload_builder::PayloadBuilderHandle,
    /// The task executor for the node.
    pub task_executor: TaskExecutor,
    /// The provider of the node.
    pub provider: BlockchainProvider<DB>,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> FullNodeComponents
    for BaseNodeContext<DB>
{
    type DB = DB;
    type Provider = BlockchainProvider<DB>;
    fn pool(&self) -> &BaseNodePool<Self::Provider> {
        &self.transaction_pool
    }

    fn evm_config(&self) -> &BaseEvmConfig {
        &self.evm_config
    }

    fn consensus(&self) -> &std::sync::Arc<base_execution_consensus::BaseBeaconConsensus> {
        &self.consensus
    }

    fn network(&self) -> &reth_network::NetworkHandle {
        &self.network
    }

    fn payload_builder_handle(&self) -> &base_execution_payload_builder::PayloadBuilderHandle {
        &self.payload_builder_handle
    }

    fn provider(&self) -> &Self::Provider {
        &self.provider
    }

    fn task_executor(&self) -> &TaskExecutor {
        &self.task_executor
    }
}
