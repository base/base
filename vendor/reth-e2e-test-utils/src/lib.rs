//! Utilities for end-to-end tests.

use std::{fmt::Debug, sync::Arc};

use base_common_consensus::BaseTxEnvelope;
use base_execution_chainspec::BaseChainSpec;
use node::NodeTestContext;
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_network_api::test_utils::PeersHandleProvider;
use reth_node_api::FullNodeComponents;
use reth_node_builder::{ComponentBuilder, FullNodeTypesAdapter, NodeAdapter, rpc::RethRpcAddOns};
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

/// Creates and connects the requested number of test nodes.
pub async fn setup<C, AO>(
    node_factory: impl Fn() -> (ComponentBuilder<crate::TmpNodeAdapter, C>, AO) + Send + Sync,
    num_nodes: usize,
    chain_spec: Arc<BaseChainSpec>,
    is_dev: bool,
    attributes_generator: impl Fn(u64) -> BasePayloadBuilderAttributes<BaseTxEnvelope>
    + Send
    + Sync
    + Copy
    + 'static,
) -> eyre::Result<(Vec<NodeHelperType<C, AO>>, Wallet)>
where
    C: Clone + Debug + Send + Sync + Unpin + 'static,
    crate::Adapter<C>: FullNodeComponents<
            DB = crate::TmpDB,
            Provider = crate::TestProvider,
            Network: PeersHandleProvider,
        >,
    AO: RethRpcAddOns<crate::Adapter<C>> + 'static,
{
    E2ETestSetupBuilder::new(num_nodes, chain_spec, attributes_generator)
        .with_node_config_modifier(move |config| config.set_dev(is_dev))
        .build(node_factory)
        .await
}

/// Creates and connects test nodes with the supplied engine configuration.
pub async fn setup_engine<C, AO>(
    node_factory: impl Fn() -> (ComponentBuilder<crate::TmpNodeAdapter, C>, AO) + Send + Sync,
    num_nodes: usize,
    chain_spec: Arc<BaseChainSpec>,
    is_dev: bool,
    tree_config: reth_node_api::TreeConfig,
    attributes_generator: impl Fn(u64) -> BasePayloadBuilderAttributes<BaseTxEnvelope>
    + Send
    + Sync
    + Copy
    + 'static,
) -> eyre::Result<(Vec<NodeHelperType<C, AO>>, Wallet)>
where
    C: Clone + Debug + Send + Sync + Unpin + 'static,
    crate::Adapter<C>: FullNodeComponents<
            DB = crate::TmpDB,
            Provider = crate::TestProvider,
            Network: PeersHandleProvider,
        >,
    AO: RethRpcAddOns<crate::Adapter<C>> + 'static,
{
    setup_engine_with_connection(
        node_factory,
        num_nodes,
        chain_spec,
        is_dev,
        tree_config,
        attributes_generator,
        true,
    )
    .await
}

/// Creates test nodes and optionally connects their networks.
pub async fn setup_engine_with_connection<C, AO>(
    node_factory: impl Fn() -> (ComponentBuilder<crate::TmpNodeAdapter, C>, AO) + Send + Sync,
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
) -> eyre::Result<(Vec<NodeHelperType<C, AO>>, Wallet)>
where
    C: Clone + Debug + Send + Sync + Unpin + 'static,
    crate::Adapter<C>: FullNodeComponents<
            DB = crate::TmpDB,
            Provider = crate::TestProvider,
            Network: PeersHandleProvider,
        >,
    AO: RethRpcAddOns<crate::Adapter<C>> + 'static,
{
    E2ETestSetupBuilder::new(num_nodes, chain_spec, attributes_generator)
        .with_tree_config_modifier(move |base| {
            tree_config.clone().with_cross_block_cache_size(base.cross_block_cache_size())
        })
        .with_node_config_modifier(move |config| config.set_dev(is_dev))
        .with_connect_nodes(connect_nodes)
        .build(node_factory)
        .await
}

// Type aliases

/// Testing database
pub type TmpDB = Arc<TempDatabase<DatabaseEnv>>;
/// Provider used by test nodes.
pub type TestProvider = BlockchainProvider<TmpDB>;

/// Provider adapter used by test nodes.
pub type TmpNodeAdapter<Provider = TestProvider> = FullNodeTypesAdapter<TmpDB, Provider>;

/// Adapter for a concrete set of test components.
pub type Adapter<C, Provider = TestProvider> = NodeAdapter<TmpNodeAdapter<Provider>, C>;

/// Context for a test node with explicit components and add-ons.
pub type NodeHelperType<C, AO, Provider = TestProvider> = NodeTestContext<Adapter<C, Provider>, AO>;
