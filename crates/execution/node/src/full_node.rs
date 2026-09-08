use std::{
    fmt::Debug,
    ops::{Deref, DerefMut},
    sync::{Arc, OnceLock},
};

use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{BaseExecutionHandle, PayloadBuilderHandle};
use base_execution_trie::ProofsProgress;
use base_node_context::FullNodeComponents;
use reth_engine_primitives::ConsensusEngineEvent;
// re-export the node api types
use reth_node_core::{
    dirs::{ChainPath, DataDirPath},
    node_config::NodeConfig,
};
use reth_provider::ChainSpecProvider;
use reth_rpc_builder::RpcServerHandle;
use reth_tasks::TaskExecutor;
use reth_tokio_util::EventSender;

use crate::{EngineShutdown, NodeAddOns, rpc::RethRpcAddOns};

/// The launched node with all components including RPC handlers.
///
/// This can be used to interact with the launched node.
#[derive(Debug)]
pub struct FullNode<Node: FullNodeComponents, AddOns: NodeAddOns<Node>> {
    /// The evm configuration.
    pub evm_config: BaseEvmConfig,
    /// The node's transaction pool.
    pub pool: base_node_context::BaseNodePool<Node::Provider>,
    /// Handle to the node's network.
    pub network: reth_network::NetworkHandle,
    /// Provider to interact with the node's database
    pub provider: Node::Provider,
    /// Handle to the node's payload builder service.
    pub payload_builder_handle: PayloadBuilderHandle,
    /// Commands submitted directly to the execution driver.
    pub execution: BaseExecutionHandle,
    /// Execution driver event stream.
    pub engine_events: EventSender<ConsensusEngineEvent>,
    /// Graceful execution shutdown and persistence.
    pub engine_shutdown: EngineShutdown,
    /// Proofs history progress registered by the execution extension before startup completes.
    pub proofs_progress: Arc<OnceLock<ProofsProgress>>,
    /// Task executor for the node.
    pub task_executor: TaskExecutor,
    /// The initial node config.
    pub config: NodeConfig,
    /// The data dir of the node.
    pub data_dir: ChainPath<DataDirPath>,
    /// The handle to launched add-ons
    pub add_ons_handle: AddOns::Handle,
}

impl<Node: FullNodeComponents, AddOns: NodeAddOns<Node>> Clone for FullNode<Node, AddOns> {
    fn clone(&self) -> Self {
        Self {
            evm_config: self.evm_config.clone(),
            pool: self.pool.clone(),
            network: self.network.clone(),
            provider: self.provider.clone(),
            payload_builder_handle: self.payload_builder_handle.clone(),
            execution: self.execution.clone(),
            engine_events: self.engine_events.clone(),
            engine_shutdown: self.engine_shutdown.clone(),
            proofs_progress: self.proofs_progress.clone(),
            task_executor: self.task_executor.clone(),
            config: self.config.clone(),
            data_dir: self.data_dir.clone(),
            add_ons_handle: self.add_ons_handle.clone(),
        }
    }
}

impl<Node, AddOns> FullNode<Node, AddOns>
where
    Node: FullNodeComponents,
    AddOns: NodeAddOns<Node>,
{
    /// Returns the chain spec of the node.
    pub fn chain_spec(&self) -> Arc<BaseChainSpec> {
        self.provider.chain_spec()
    }
}

impl<Node, AddOns> FullNode<Node, AddOns>
where
    Node: FullNodeComponents,
    AddOns: RethRpcAddOns<Node>,
{
    /// Returns the [`RpcServerHandle`] to the started rpc server.
    pub const fn rpc_server_handle(&self) -> &RpcServerHandle {
        &self.add_ons_handle.rpc_server_handles.rpc
    }
}

impl<Node: FullNodeComponents, AddOns: NodeAddOns<Node>> Deref for FullNode<Node, AddOns> {
    type Target = AddOns::Handle;

    fn deref(&self) -> &Self::Target {
        &self.add_ons_handle
    }
}

impl<Node: FullNodeComponents, AddOns: NodeAddOns<Node>> DerefMut for FullNode<Node, AddOns> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.add_ons_handle
    }
}
