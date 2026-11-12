//! RPC integration tests.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use base_node_core::{BaseNode, TestNodeRuntime};
use reth_network::types::NatResolver;
use reth_node_builder::{NodeBuilder, NodeHandle};
use reth_node_core::{
    args::{NetworkArgs, RpcServerArgs},
    node_config::NodeConfig,
};
use reth_rpc_api::servers::AdminApiServer;

// <https://github.com/paradigmxyz/reth/issues/19765>
#[tokio::test(flavor = "multi_thread")]
async fn test_admin_external_ip() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut exec = TestNodeRuntime::default();

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

    let launch = NodeBuilder::new(node_config)
        .testing_node(exec.runtime.clone())
        .node(BaseNode::default())
        .launch();
    let NodeHandle { node, node_exit_future: _ } = exec.runtime.handle().spawn(launch).await??;

    exec.rocksdb_path = Some(node.config.datadir().rocksdb());

    let api = node.add_ons_handle.admin_api();

    let info = api.node_info().await.unwrap();

    assert_eq!(info.ip, external_ip);

    Ok(())
}
