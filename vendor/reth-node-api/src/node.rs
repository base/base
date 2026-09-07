//! Traits for configuring a node.

use std::{fmt::Debug, future::Future};

use alloy_rpc_types_engine::JwtSecret;
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_engine_primitives::{ConsensusEngineEvent, ConsensusEngineHandle};
use reth_evm::BaseEvmConfig;
use reth_node_core::node_config::NodeConfig;
use reth_payload_builder::PayloadBuilderHandle;
use reth_provider::FullProvider;
use reth_tasks::TaskExecutor;
use reth_tokio_util::EventSender;

/// Base's transaction pool with its production disk blob store.
pub type BaseNodePool<Provider> = base_execution_txpool::BaseTransactionPool<
    Provider,
    reth_transaction_pool::blobstore::DiskFileBlobStore,
>;

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

/// Context passed to [`NodeAddOns::launch_add_ons`],
#[derive(Debug, Clone)]
pub struct AddOnsContext<'a, N: FullNodeComponents> {
    /// Node with all configured components.
    pub node: N,
    /// Node configuration.
    pub config: &'a NodeConfig,
    /// Handle to the beacon consensus engine.
    pub beacon_engine_handle: ConsensusEngineHandle,
    /// Notification channel for engine API events
    pub engine_events: EventSender<ConsensusEngineEvent>,
    /// JWT secret for the node.
    pub jwt_secret: JwtSecret,
}

/// Customizable node add-on types.
///
/// This trait defines the interface for extending a node with additional functionality beyond
/// the core [`FullNodeComponents`]. It provides a way to launch supplementary services such as
/// RPC servers, monitoring, external integrations, or any custom functionality that builds on
/// top of the core node components.
///
/// ## Purpose
///
/// The `NodeAddOns` trait serves as an extension point in the node builder architecture,
/// allowing developers to:
/// - Define custom services that run alongside the main node
/// - Access all node components and configuration during initialization
/// - Return a handle for managing the launched services (e.g. handle to rpc server)
///
/// ## How it fits into `NodeBuilder`
///
/// In the node builder pattern, add-ons are the final layer that gets applied after all core
/// components are configured and started. The builder flow typically follows:
///
/// 1. Configure the database and state provider
/// 2. Build [`FullNodeComponents`] (consensus, networking, transaction pool, etc.)
/// 3. Launch [`NodeAddOns`] with access to all components via [`AddOnsContext`]
///
/// ## Primary Use Case
///
/// The primary use of this trait is to launch RPC servers that provide external API access to
/// the node. For Ethereum nodes, this typically includes two main servers: the regular RPC
/// server (HTTP/WS/IPC) that handles user requests and the authenticated Engine API server
/// that communicates with the consensus layer. The returned handle contains the necessary
/// endpoints and control mechanisms for these servers, allowing the node to serve JSON-RPC
/// requests and participate in consensus. While RPC is the main use case, the trait is
/// intentionally flexible to support other kinds of add-ons such as monitoring, indexing, or
/// custom protocol extensions.
///
/// ## Context Access
///
/// The [`AddOnsContext`] provides access to:
/// - All node components via the `node` field
/// - Node configuration
/// - Engine API handles for consensus layer communication
/// - JWT secrets for authenticated endpoints
///
/// This ensures add-ons can integrate deeply with the node while maintaining clean separation
/// of concerns.
pub trait NodeAddOns<N: FullNodeComponents>: Send {
    /// Handle to add-ons.
    ///
    /// This type is returned by [`launch_add_ons`](Self::launch_add_ons) and represents a
    /// handle to the launched services. It must be `Clone` to allow multiple components to
    /// hold references and should provide methods to interact with the running services.
    ///
    /// For RPC add-ons, this typically includes:
    /// - Server handles to access local addresses and shutdown methods
    /// - RPC module registry for runtime inspection of available methods
    /// - Configured middleware and transport-specific settings
    /// - For Engine API implementations, this also includes handles for consensus layer
    ///   communication
    type Handle: Send + Sync + Clone;

    /// Configures and launches the add-ons.
    ///
    /// This method is called once during node startup after all core components are initialized.
    /// It receives an [`AddOnsContext`] that provides access to:
    ///
    /// - The fully configured node with all its components
    /// - Node configuration for reading settings
    /// - Engine API handles for consensus layer communication
    /// - JWT secrets for setting up authenticated endpoints (if any).
    ///
    /// The implementation should:
    /// 1. Use the context to configure the add-on services
    /// 2. Launch any background tasks using the node's task executor
    /// 3. Return a handle that allows interaction with the launched services
    ///
    /// # Errors
    ///
    /// This method may fail if the add-ons cannot be properly configured or launched,
    /// for example due to port binding issues or invalid configuration.
    fn launch_add_ons(
        self,
        ctx: AddOnsContext<'_, N>,
    ) -> impl Future<Output = eyre::Result<Self::Handle>> + Send;
}

impl<N: FullNodeComponents> NodeAddOns<N> for () {
    type Handle = ();

    async fn launch_add_ons(self, _components: AddOnsContext<'_, N>) -> eyre::Result<Self::Handle> {
        Ok(())
    }
}
