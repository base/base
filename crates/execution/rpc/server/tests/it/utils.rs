use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use base_common_runtime_tasks::EventSender;
use base_common_runtime_tasks::Runtime;
use base_execution_evm_blocks::BaseBeaconConsensus;
use base_execution_rpc_server::{
    RpcRegistryInner, RpcServerConfig, RpcServerHandle, TransportRpcModuleConfig,
};
use reth_primitives_traits::SignedTransaction;

/// Localhost with port 0 so a free port is used.
pub const fn test_address() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
}

/// Launches a new server with http only with the given modules
pub async fn launch_http() -> RpcServerHandle {
    let mut registry = test_rpc_registry().await;
    let server = registry.create_transport_rpc_modules(TransportRpcModuleConfig::set_http());
    RpcServerConfig::http(Default::default())
        .with_http_address(test_address())
        .start(&server)
        .await
        .unwrap()
}

/// Launches a new server with ws only with the given modules
pub async fn launch_ws() -> RpcServerHandle {
    let mut registry = test_rpc_registry().await;
    let server = registry.create_transport_rpc_modules(TransportRpcModuleConfig::set_ws());
    RpcServerConfig::ws(Default::default())
        .with_ws_address(test_address())
        .start(&server)
        .await
        .unwrap()
}

/// Launches a new server with http and ws and with the given modules
pub async fn launch_http_ws() -> RpcServerHandle {
    let mut registry = test_rpc_registry().await;
    let server =
        registry.create_transport_rpc_modules(TransportRpcModuleConfig::set_ws().with_http());
    RpcServerConfig::ws(Default::default())
        .with_ws_address(test_address())
        .with_ws_address(test_address())
        .with_http(Default::default())
        .with_http_address(test_address())
        .start(&server)
        .await
        .unwrap()
}

/// Launches a new server with http and ws and with the given modules on the same port.
pub async fn launch_http_ws_same_port() -> RpcServerHandle {
    let mut registry = test_rpc_registry().await;
    let server =
        registry.create_transport_rpc_modules(TransportRpcModuleConfig::set_ws().with_http());
    let addr = test_address();
    RpcServerConfig::ws(Default::default())
        .with_ws_address(addr)
        .with_http(Default::default())
        .with_http_address(addr)
        .start(&server)
        .await
        .unwrap()
}

/// Returns an [`RpcRegistryInner`] with testing components.
pub async fn test_rpc_registry() -> RpcRegistryInner {
    let mock = base_execution_state_provider::test_utils::MockEthProvider::default();
    let transaction = base_execution_txpool::test_utils::TransactionBuilder::default()
        .signer(alloy_primitives::B256::repeat_byte(1))
        .into_eip1559();
    let sender = transaction.try_into_recovered().unwrap().signer();
    mock.add_account(
        sender,
        base_execution_state_provider::test_utils::ExtendedAccount::new(
            0,
            alloy_primitives::U256::MAX,
        ),
    );
    let mut context = base_execution_rpc_handlers::test_utils::RpcTestUtils::context(mock);
    let manager =
        base_execution_network_service::NetworkConfig::builder_with_rng_secret_key(Runtime::test())
            .disable_discovery()
            .listener_addr(test_address())
            .build(context.provider.clone())
            .manager()
            .await
            .expect("local fixture network");
    context.network = manager.handle().clone();
    tokio::spawn(manager);
    let eth_api =
        base_execution_rpc_handlers::EthApiBuilder::new_with_components(context.clone()).build();
    RpcRegistryInner::new(
        context.provider,
        context.pool,
        context.network,
        Runtime::test(),
        std::sync::Arc::new(BaseBeaconConsensus::noop()),
        Default::default(),
        context.evm_config,
        eth_api,
        EventSender::new(1),
    )
}
