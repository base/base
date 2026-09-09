//! RPC integration tests.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use base_node_core::{BaseNode, NodeBuilder, NodeHandle};
use reth_network::types::NatResolver;
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
        .with_chain(Arc::new(BaseChainSpec::mainnet()))
        .with_network(network_args)
        .with_rpc(RpcServerArgs::default().with_unused_ports().with_http());

    let add_ons: base_node_core::BaseNodeAddOns = BaseNode::default().add_ons_builder().build();

    let NodeHandle { node, node_exit_future: _ } = NodeBuilder::new(node_config)
        .testing_node(exec)
        .with_components(BaseNode::default().components().into_builder())
        .with_add_ons(add_ons)
        .launch()
        .await?;

    assert!(node.rpc_server_handle().http_local_addr().is_some());

    let api = node.add_ons_handle.admin_api();

    let info = api.node_info().await.unwrap();

    assert_eq!(info.ip, external_ip);

    Ok(())
}
