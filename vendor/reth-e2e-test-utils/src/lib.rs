//! Utilities for end-to-end tests.

use std::sync::Arc;

use base_common_consensus::BaseTxEnvelope;
use base_execution_chainspec::BaseChainSpec;
use node::NodeTestContext;
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_node_builder::{
    FullNodeTypesAdapter, Node, NodeAdapter, NodeBuiltComponents, NodeTypesWithDBAdapter,
};
use reth_payload_primitives::BasePayloadBuilderAttributes;
use reth_provider::providers::BlockchainProvider;
use wallet::Wallet;

/// Wrapper type to create test nodes
pub mod node;
pub mod testsuite;

/// Helper for transaction operations
pub mod transaction;

/// Helper type to yield accounts from mnemonic
pub mod wallet;

/// Helper for payload operations
mod payload;
pub use payload::PayloadTestContext;

/// Helper for network operations
mod network;
pub use network::NetworkTestContext;

/// Helper for rpc operations
mod rpc;

/// Utilities for creating and writing RLP test data
pub mod test_rlp_utils;

/// Builder for configuring test node setups
mod setup_builder;
pub use setup_builder::E2ETestSetupBuilder;

/// Creates the initial setup with `num_nodes` started and interconnected.
pub async fn setup<N>(
    num_nodes: usize,
    chain_spec: Arc<BaseChainSpec>,
    is_dev: bool,
    attributes_generator: impl Fn(u64) -> BasePayloadBuilderAttributes<BaseTxEnvelope>
    + Send
    + Sync
    + Copy
    + 'static,
) -> eyre::Result<(Vec<NodeHelperType<N>>, Wallet)>
where
    N: Default
        + reth_node_builder::Node<
            crate::TmpNodeAdapter,
            Network: reth_network_api::test_utils::PeersHandleProvider,
            AddOns: reth_node_builder::rpc::RethRpcAddOns<crate::Adapter<N>>
                        + reth_node_builder::rpc::EngineValidatorAddOn<crate::Adapter<N>>,
        >,
{
    E2ETestSetupBuilder::new(num_nodes, chain_spec, attributes_generator)
        .with_node_config_modifier(move |config| config.set_dev(is_dev))
        .build::<N>()
        .await
}

/// Creates the initial setup with `num_nodes` started and interconnected.
pub async fn setup_engine<N>(
    num_nodes: usize,
    chain_spec: Arc<BaseChainSpec>,
    is_dev: bool,
    tree_config: reth_node_api::TreeConfig,
    attributes_generator: impl Fn(u64) -> BasePayloadBuilderAttributes<BaseTxEnvelope>
    + Send
    + Sync
    + Copy
    + 'static,
) -> eyre::Result<(Vec<NodeHelperType<N, BlockchainProvider<NodeTypesWithDBAdapter<TmpDB>>>>, Wallet)>
where
    N: Default
        + reth_node_builder::Node<
            crate::TmpNodeAdapter,
            Network: reth_network_api::test_utils::PeersHandleProvider,
            AddOns: reth_node_builder::rpc::RethRpcAddOns<crate::Adapter<N>>
                        + reth_node_builder::rpc::EngineValidatorAddOn<crate::Adapter<N>>,
        >,
{
    setup_engine_with_connection::<N>(
        num_nodes,
        chain_spec,
        is_dev,
        tree_config,
        attributes_generator,
        true,
    )
    .await
}

/// Creates the initial setup with `num_nodes` started and optionally interconnected.
pub async fn setup_engine_with_connection<N>(
    num_nodes: usize,
    chain_spec: Arc<BaseChainSpec>,
    is_dev: bool,
    tree_config: reth_node_api::TreeConfig,
    attributes_generator: impl Fn(u64) -> BasePayloadBuilderAttributes<BaseTxEnvelope>
    + Send
    + Sync
    + Copy
    + 'static,
    connect_nodes: bool,
) -> eyre::Result<(Vec<NodeHelperType<N, BlockchainProvider<NodeTypesWithDBAdapter<TmpDB>>>>, Wallet)>
where
    N: Default
        + reth_node_builder::Node<
            crate::TmpNodeAdapter,
            Network: reth_network_api::test_utils::PeersHandleProvider,
            AddOns: reth_node_builder::rpc::RethRpcAddOns<crate::Adapter<N>>
                        + reth_node_builder::rpc::EngineValidatorAddOn<crate::Adapter<N>>,
        >,
{
    E2ETestSetupBuilder::new(num_nodes, chain_spec, attributes_generator)
        .with_tree_config_modifier(move |base| {
            // Apply caller's tree_config but preserve the small cache size from base
            tree_config.clone().with_cross_block_cache_size(base.cross_block_cache_size())
        })
        .with_node_config_modifier(move |config| config.set_dev(is_dev))
        .with_connect_nodes(connect_nodes)
        .build::<N>()
        .await
}

// Type aliases

/// Testing database
pub type TmpDB = Arc<TempDatabase<DatabaseEnv>>;
/// Provider adapter used by test nodes.
pub type TmpNodeAdapter<Provider = BlockchainProvider<NodeTypesWithDBAdapter<TmpDB>>> =
    FullNodeTypesAdapter<TmpDB, Provider>;

/// Type alias for a `NodeAdapter`
pub type Adapter<N, Provider = BlockchainProvider<NodeTypesWithDBAdapter<TmpDB>>> =
    NodeAdapter<TmpNodeAdapter<Provider>, NodeBuiltComponents<TmpNodeAdapter<Provider>, N>>;

/// Type alias for a type of `NodeHelper`
pub type NodeHelperType<N, Provider = BlockchainProvider<NodeTypesWithDBAdapter<TmpDB>>> =
    NodeTestContext<Adapter<N, Provider>, <N as Node<TmpNodeAdapter<Provider>>>::AddOns>;
