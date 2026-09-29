//! RPC integration tests.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use base_node_core::BaseNode;
use reth_network::types::NatResolver;
use reth_node_builder::{NodeBuilder, NodeHandle};
use reth_node_core::{
    args::{NetworkArgs, RpcServerArgs},
    node_config::NodeConfig,
};
use reth_rpc_api::servers::AdminApiServer;
use reth_tasks::Runtime;

// <https://github.com/paradigmxyz/reth/issues/19765>
#[tokio::test(flavor = "multi_thread")]
async fn test_admin_external_ip() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let exec = Runtime::test();

    let external_ip = "10.64.128.71".parse().unwrap();
    // Node setup
    let mut network_args = NetworkArgs::default()
        .with_unused_ports()
        .with_nat_resolver(NatResolver::ExternalIp(external_ip));
    network_args.discovery.discv5_port = Some(0);
    network_args.discovery.discv5_port_ipv6 = Some(0);
    let node_config = NodeConfig::test()
        .map_chain(Arc::new(BaseChainSpec::mainnet()))
        .with_network(network_args)
        .with_rpc(RpcServerArgs::default().with_unused_ports().with_http());

    let NodeHandle { node, node_exit_future: _ } =
        NodeBuilder::new(node_config).testing_node(exec).node(BaseNode::default()).launch().await?;

    let api = node.add_ons_handle.admin_api();

    let info = api.node_info().await.unwrap();

    assert_eq!(info.ip, external_ip);

    Ok(())
}

async fn launch_pool_node(txpool_enabled: bool) -> eyre::Result<bool> {
    let mut network_args = NetworkArgs::default().with_unused_ports();
    network_args.discovery.discv5_port = Some(0);
    network_args.discovery.discv5_port_ipv6 = Some(0);
    let node_config = NodeConfig::test()
        .map_chain(Arc::new(BaseChainSpec::mainnet()))
        .with_network(network_args)
        .with_rpc(RpcServerArgs::default().with_unused_ports().with_http());

    let NodeHandle { node, node_exit_future: _ } = NodeBuilder::new(node_config)
        .testing_node(Runtime::test())
        .node(BaseNode::default().with_txpool_enabled(txpool_enabled))
        .launch()
        .await?;

    Ok(node.pool.is_noop())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_txpool_is_noop_unless_enabled() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    assert!(launch_pool_node(false).await?, "default node must run a noop pool");
    assert!(!launch_pool_node(true).await?, "enabled node must run the real pool");

    Ok(())
}
