use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use base_execution_consensus::BaseBeaconConsensus;
use reth_primitives_traits::SignedTransaction;
use reth_rpc_builder::{
    RpcModuleBuilder, RpcServerConfig, RpcServerHandle, TransportRpcModuleConfig,
};
use reth_rpc_server_types::RpcModuleSelection;
use reth_tasks::Runtime;
use reth_tokio_util::EventSender;

/// Localhost with port 0 so a free port is used.
pub const fn test_address() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
}

/// Launches a new server with http only with the given modules
pub async fn launch_http(modules: impl Into<RpcModuleSelection>) -> RpcServerHandle {
    let builder = test_rpc_builder().await;
    let eth_api = builder.eth_api_builder().build();
    let server =
        builder.build(TransportRpcModuleConfig::set_http(modules), eth_api, EventSender::new(1));
    RpcServerConfig::http(Default::default())
        .with_http_address(test_address())
        .start(&server)
        .await
        .unwrap()
}

/// Launches a new server with ws only with the given modules
pub async fn launch_ws(modules: impl Into<RpcModuleSelection>) -> RpcServerHandle {
    let builder = test_rpc_builder().await;
    let eth_api = builder.eth_api_builder().build();
    let server =
        builder.build(TransportRpcModuleConfig::set_ws(modules), eth_api, EventSender::new(1));
    RpcServerConfig::ws(Default::default())
        .with_ws_address(test_address())
        .start(&server)
        .await
        .unwrap()
}

/// Launches a new server with http and ws and with the given modules
pub async fn launch_http_ws(modules: impl Into<RpcModuleSelection>) -> RpcServerHandle {
    let builder = test_rpc_builder().await;
    let eth_api = builder.eth_api_builder().build();
    let modules = modules.into();
    let server = builder.build(
        TransportRpcModuleConfig::set_ws(modules.clone()).with_http(modules),
        eth_api,
        EventSender::new(1),
    );
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
pub async fn launch_http_ws_same_port(modules: impl Into<RpcModuleSelection>) -> RpcServerHandle {
    let builder = test_rpc_builder().await;
    let modules = modules.into();
    let eth_api = builder.eth_api_builder().build();
    let server = builder.build(
        TransportRpcModuleConfig::set_ws(modules.clone()).with_http(modules),
        eth_api,
        EventSender::new(1),
    );
    let addr = test_address();
    RpcServerConfig::ws(Default::default())
        .with_ws_address(addr)
        .with_http(Default::default())
        .with_http_address(addr)
        .start(&server)
        .await
        .unwrap()
}

/// Returns an [`RpcModuleBuilder`] with testing components.
pub async fn test_rpc_builder() -> RpcModuleBuilder {
    let mock = reth_provider::test_utils::MockEthProvider::default();
    let transaction = base_execution_txpool::test_utils::TransactionBuilder::default()
        .signer(alloy_primitives::B256::repeat_byte(1))
        .into_eip1559();
    let sender = transaction.try_into_recovered().unwrap().signer();
    mock.add_account(
        sender,
        reth_provider::test_utils::ExtendedAccount::new(0, alloy_primitives::U256::MAX),
    );
    let mut context = base_execution_rpc::test_utils::RpcTestUtils::context(mock);
    let manager = reth_network::NetworkConfig::builder_with_rng_secret_key(Runtime::test())
        .disable_discovery()
        .listener_addr(test_address())
        .build(context.provider.clone())
        .manager()
        .await
        .expect("local fixture network");
    context.network = manager.handle().clone();
    tokio::spawn(manager);
    RpcModuleBuilder::new(
        context.provider,
        context.pool,
        context.network,
        Runtime::test(),
        context.evm_config,
        std::sync::Arc::new(BaseBeaconConsensus::noop()),
    )
}
