use std::{
    fmt::Debug,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use base_execution_chainspec::BaseChainSpec;
use reth_evm::BaseEvmConfig;
use reth_node_api::FullNodeComponents;
// re-export the node api types
pub use reth_node_api::FullNodeTypes;
use reth_node_core::{
    dirs::{ChainPath, DataDirPath},
    node_config::NodeConfig,
};
use reth_payload_builder::PayloadBuilderHandle;
use reth_provider::ChainSpecProvider;
use reth_rpc_api::EngineApiClient;
use reth_rpc_builder::{RpcServerHandle, auth::AuthServerHandle};
use reth_tasks::TaskExecutor;

use crate::{NodeAddOns, rpc::RethRpcAddOns};

/// The launched node with all components including RPC handlers.
///
/// This can be used to interact with the launched node.
#[derive(Debug)]
pub struct FullNode<Node: FullNodeComponents, AddOns: NodeAddOns<Node>> {
    /// The evm configuration.
    pub evm_config: BaseEvmConfig,
    /// The node's transaction pool.
    pub pool: reth_node_api::BaseNodePool<Node::Provider>,
    /// Handle to the node's network.
    pub network: reth_network::NetworkHandle,
    /// Provider to interact with the node's database
    pub provider: Node::Provider,
    /// Handle to the node's payload builder service.
    pub payload_builder_handle: PayloadBuilderHandle,
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

    /// Returns the [`AuthServerHandle`] to the started authenticated engine API server.
    pub const fn auth_server_handle(&self) -> &AuthServerHandle {
        &self.add_ons_handle.rpc_server_handles.auth
    }
}

impl<Node, AddOns> FullNode<Node, AddOns>
where
    Node: FullNodeComponents,
    AddOns: RethRpcAddOns<Node>,
{
    /// Returns the [`EngineApiClient`] interface for the authenticated engine API.
    ///
    /// This will send authenticated http requests to the node's auth server.
    pub fn engine_http_client(&self) -> impl EngineApiClient + use<Node, AddOns> {
        self.auth_server_handle().http_client()
    }

    /// Returns the [`EngineApiClient`] interface for the authenticated engine API.
    ///
    /// This will send authenticated ws requests to the node's auth server.
    pub async fn engine_ws_client(&self) -> impl EngineApiClient + use<Node, AddOns> {
        self.auth_server_handle().ws_client().await
    }

    /// Returns the [`EngineApiClient`] interface for the authenticated engine API.
    ///
    /// This will send not authenticated IPC requests to the node's auth server.
    #[cfg(unix)]
    pub async fn engine_ipc_client(&self) -> Option<impl EngineApiClient + use<Node, AddOns>> {
        self.auth_server_handle().ipc_client().await
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
