//! Local node setup with Base Sepolia chainspec

use std::{any::Any, fmt, net::SocketAddr, path::PathBuf, sync::Arc};

use alloy_rpc_client::RpcClient;
use base_common_chain_config::BaseChainSpec;
use base_common_client_ethereum::Base;
use base_common_client_ethereum::RootProvider;
use base_common_runtime_tasks::Runtime;
use base_execution_state_database::{
    ClientVersion, DatabaseEnv, init_db, mdbx::DatabaseArguments, test_utils::tempdir_path,
};
use base_node_core::{BaseNode, NodeConfig, NodeHandle, RollupArgs};
use eyre::Result;
use reth_node_core::{
    args::{DatadirArgs, DiscoveryArgs, NetworkArgs, RpcServerArgs},
    dirs::{DataDirPath, MaybePlatformPath},
    exit::NodeExitFuture,
};

use crate::test_utils::engine::EngineApi;

/// Convenience alias for the local blockchain provider type.
pub type LocalNodeProvider = base_execution_state_provider::providers::BlockchainProvider;

/// Handle to a launched local node along with the resources required to keep it alive.
pub struct LocalNode {
    /// In-process execution services.
    pub execution: base_execution_payload_builder::BaseExecutionHandle,
    /// Execution network and synchronization status.
    pub network: base_execution_network_service::NetworkHandle,
    /// HTTP API address of the local node.
    pub http_api_addr: SocketAddr,
    /// WebSocket API address of the local node.
    pub ws_api_addr: SocketAddr,
    provider: LocalNodeProvider,
    _node_exit_future: NodeExitFuture,
    _node: Box<dyn Any + Sync + Send>,
    _runtime: Runtime,
    _db_path: PathBuf,
}

impl Drop for LocalNode {
    fn drop(&mut self) {
        // Clean up the temporary database directory
        let _ = std::fs::remove_dir_all(&self._db_path);
    }
}

impl fmt::Debug for LocalNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalNode")
            .field("http_api_addr", &self.http_api_addr)
            .field("ws_api_addr", &self.ws_api_addr)
            .finish_non_exhaustive()
    }
}

impl LocalNode {
    /// Launch a new local node with the provided extensions and chain spec.
    pub async fn new(
        services: base_node_core::NodeServices,
        rpc: base_node_core::BaseRpcServices,
        chain_spec: Arc<BaseChainSpec>,
    ) -> Result<Self> {
        let exec = Runtime::test();

        let network_config = NetworkArgs {
            discovery: DiscoveryArgs { disable_discovery: true, ..DiscoveryArgs::default() },
            ..NetworkArgs::default()
        };

        let rpc_args = RpcServerArgs::default().with_unused_ports().with_http().with_ws();

        let base_node = BaseNode::new(RollupArgs::default());

        let (db, db_path) = Self::create_test_database()?;

        let mut node_config = NodeConfig::new(Arc::clone(&chain_spec))
            .with_network(network_config)
            .with_rpc(rpc_args)
            .with_unused_ports();

        // The Engine API test harness builds blocks back-to-back and expects each canonical head to
        // be immediately usable as the next payload parent. Persist canonical blocks immediately so
        // follow-up payload validation can resolve parent headers from the database-backed paths
        // that reth 2.3.0 now consults during state-root/proof work.
        node_config.engine.persistence_threshold = 0;

        let datadir_path = MaybePlatformPath::<DataDirPath>::from(db_path.clone());
        node_config = node_config
            .with_datadir_args(DatadirArgs { datadir: datadir_path, ..Default::default() });

        let mut builder = base_node_core::NodeLaunch::new(node_config.clone(), db, exec.clone());
        builder.base = base_node;
        builder.rpc = rpc;
        builder.services = services;

        let NodeHandle { node: node_handle, node_exit_future } = builder.launch().await?;

        let http_api_addr = node_handle
            .rpc_server_handle()
            .http_local_addr()
            .ok_or_else(|| eyre::eyre!("HTTP RPC server failed to bind to address"))?;

        let ws_api_addr = node_handle
            .rpc_server_handle()
            .ws_local_addr()
            .ok_or_else(|| eyre::eyre!("Failed to get websocket api address"))?;

        let provider = node_handle.provider().clone();

        Ok(Self {
            execution: node_handle.execution.clone(),
            network: node_handle.network.clone(),
            http_api_addr,
            ws_api_addr,
            provider,
            _node_exit_future: node_exit_future,
            _node: Box::new(node_handle),
            _runtime: exec,
            _db_path: db_path,
        })
    }

    /// Creates a test database with a 100 MB map size (vs reth's default 8 TB).
    fn create_test_database() -> Result<(DatabaseEnv, PathBuf)> {
        let path = tempdir_path();
        let args = DatabaseArguments::new(ClientVersion::default())
            .with_geometry_max_size(Some(100 * 1024 * 1024));
        let db = init_db(&path, args).expect("Failed to create test database");
        Ok((db, path))
    }

    /// Create an HTTP provider pointed at the node's public RPC endpoint.
    pub fn provider(&self) -> Result<RootProvider<Base>> {
        let url = format!("http://{}", self.http_api_addr);
        let client = RpcClient::builder().http(url.parse()?);
        Ok(RootProvider::<Base>::new(client))
    }

    /// HTTP RPC address for the local node.
    pub const fn http_addr(&self) -> SocketAddr {
        self.http_api_addr
    }

    /// Access the local execution driver and payload builder.
    pub fn engine_api(&self) -> EngineApi {
        EngineApi { execution: self.execution.clone() }
    }

    /// Clone the underlying blockchain provider so callers can inspect chain state.
    pub fn blockchain_provider(&self) -> LocalNodeProvider {
        self.provider.clone()
    }

    /// Websocket URL for the local node.
    pub fn ws_url(&self) -> String {
        format!("ws://{}", self.ws_api_addr)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_eips::eip7685::Requests;
    use alloy_primitives::B256;
    use base_common_chain_config::BaseChainSpec;
    use base_common_runtime_tasks::Runtime;
    use base_common_types_payload::{
        BaseExecutionPayloadEnvelopeV4, BasePayloadAttributes, ForkchoiceState, PayloadAttributes,
    };
    use base_execution_payload_types::BasePayloadBuilderAttributes;
    use base_execution_state_provider::{DatabaseProviderROFactory, HeaderProvider};
    use base_node_core::{BaseNode, NodeConfig, RollupArgs};
    use base_testing_support::build_test_genesis;
    use reth_node_core::{
        args::{DatadirArgs, DiscoveryArgs, NetworkArgs},
        dirs::{DataDirPath, MaybePlatformPath},
    };

    use super::LocalNode;
    use crate::test_utils::engine::EngineApi;

    #[tokio::test]
    async fn execution_builds_and_persists_with_all_rpc_disabled() {
        let chain = Arc::new(BaseChainSpec::from(build_test_genesis()));
        let genesis = chain.genesis_header();
        let head = genesis.hash_slow();
        let attributes = BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: genesis.timestamp + 2,
                withdrawals: Some(Vec::new()),
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            gas_limit: Some(genesis.gas_limit),
            eip_1559_params: Some(Default::default()),
            min_base_fee: Some(0),
            no_tx_pool: Some(true),
            ..Default::default()
        };
        let (db, path) = LocalNode::create_test_database().unwrap();
        let runtime = Runtime::test();
        let base = BaseNode::new(RollupArgs::default());
        let config = NodeConfig::new(chain)
            .with_network(NetworkArgs {
                discovery: DiscoveryArgs { disable_discovery: true, ..Default::default() },
                ..Default::default()
            })
            .with_unused_ports()
            .with_datadir_args(DatadirArgs {
                datadir: MaybePlatformPath::<DataDirPath>::from(path.clone()),
                ..Default::default()
            });
        let mut launch = base_node_core::NodeLaunch::new(config, db, runtime.clone());
        launch.base = base;
        let handle = launch.launch().await.unwrap();
        assert!(handle.node.rpc_server_handle().http_local_addr().is_none());
        assert!(handle.node.rpc_server_handle().ws_local_addr().is_none());
        let execution = &handle.node.execution;
        let started = execution
            .update_forkchoice(
                ForkchoiceState::same_hash(head),
                Some(BasePayloadBuilderAttributes::try_new(head, attributes, 3).unwrap()),
            )
            .await
            .unwrap();
        let built = execution.resolve_payload(started.payload_id.unwrap()).await.unwrap();
        let hash = built.block().hash();
        let payload: BaseExecutionPayloadEnvelopeV4 = built.into();
        let imported = EngineApi { execution: execution.clone() }
            .new_payload(payload.execution_payload, Vec::new(), B256::ZERO, Requests::default())
            .await
            .unwrap();
        assert!(imported.is_valid());
        let canonical =
            execution.update_forkchoice(ForkchoiceState::same_hash(hash), None).await.unwrap();
        assert!(canonical.payload_status.is_valid());
        let done = handle.node.engine_shutdown.shutdown().unwrap();
        tokio::time::timeout(Duration::from_secs(10), done).await.unwrap().unwrap();
        assert_eq!(
            handle
                .node
                .provider
                .database_provider_ro()
                .unwrap()
                .header_by_number(1)
                .unwrap()
                .unwrap()
                .hash_slow(),
            hash
        );
        drop(handle);
        drop(runtime);
        std::fs::remove_dir_all(path).unwrap();
    }
}
