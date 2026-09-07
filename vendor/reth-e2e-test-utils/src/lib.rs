//! Utilities for end-to-end tests.

use std::sync::Arc;

use base_common_consensus::BaseTxEnvelope;
use base_execution_chainspec::BaseChainSpec;
use node::NodeTestContext;
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_network_api::test_utils::PeersHandleProvider;
use reth_node_builder::{
    FullNodeTypesAdapter, Node, NodeAdapter, NodeComponents, NodeTypesWithDBAdapter,
    components::NodeComponentsBuilder,
    rpc::{EngineValidatorAddOn, RethRpcAddOns},
};
use reth_payload_primitives::BasePayloadBuilderAttributes;
use reth_provider::providers::{BlockchainProvider, NodeTypesForProvider};
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
    N: NodeBuilderHelper,
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
) -> eyre::Result<(
    Vec<NodeHelperType<N, BlockchainProvider<NodeTypesWithDBAdapter<N, TmpDB>>>>,
    Wallet,
)>
where
    N: NodeBuilderHelper,
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
) -> eyre::Result<(
    Vec<NodeHelperType<N, BlockchainProvider<NodeTypesWithDBAdapter<N, TmpDB>>>>,
    Wallet,
)>
where
    N: NodeBuilderHelper,
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
type TmpNodeAdapter<N, Provider = BlockchainProvider<NodeTypesWithDBAdapter<N, TmpDB>>> =
    FullNodeTypesAdapter<N, TmpDB, Provider>;

/// Type alias for a `NodeAdapter`
pub type Adapter<N, Provider = BlockchainProvider<NodeTypesWithDBAdapter<N, TmpDB>>> = NodeAdapter<
    TmpNodeAdapter<N, Provider>,
    <<N as Node<TmpNodeAdapter<N, Provider>>>::ComponentsBuilder as NodeComponentsBuilder<
        TmpNodeAdapter<N, Provider>,
    >>::Components,
>;

/// Type alias for a type of `NodeHelper`
pub type NodeHelperType<N, Provider = BlockchainProvider<NodeTypesWithDBAdapter<N, TmpDB>>> =
    NodeTestContext<Adapter<N, Provider>, <N as Node<TmpNodeAdapter<N, Provider>>>::AddOns>;

/// Helper trait to simplify bounds when calling setup functions.
pub trait NodeBuilderHelper
where
    Self: Default
        + NodeTypesForProvider
        + Node<
            TmpNodeAdapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
            ComponentsBuilder: NodeComponentsBuilder<
                TmpNodeAdapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
                Components: NodeComponents<
                    TmpNodeAdapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
                    Network: PeersHandleProvider,
                >,
            >,
            AddOns: RethRpcAddOns<
                Adapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
            > + EngineValidatorAddOn<
                Adapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
            >,
        >,
{
}

impl<T> NodeBuilderHelper for T where
    Self: Default
        + NodeTypesForProvider
        + Node<
            TmpNodeAdapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
            ComponentsBuilder: NodeComponentsBuilder<
                TmpNodeAdapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
                Components: NodeComponents<
                    TmpNodeAdapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
                    Network: PeersHandleProvider,
                >,
            >,
            AddOns: RethRpcAddOns<
                Adapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
            > + EngineValidatorAddOn<
                Adapter<Self, BlockchainProvider<NodeTypesWithDBAdapter<Self, TmpDB>>>,
            >,
        >
{
}
