//! Type aliases for the Base node builder.

use base_common_consensus::{
    BasePooledTransaction as ConsensusPooledTransaction, BaseTransactionSigned,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::BasePooledTransaction;
use base_node_core::{
    BaseConsensusBuilder, BaseExecutorBuilder, BaseNetworkBuilder,
    node::{BasePayloadBuilder, BasePoolBuilder},
};
use reth_db::DatabaseEnv;
use reth_node_builder::{
    FullNodeTypesAdapter, Node, NodeBuilder, NodeTypesWithDBAdapter, WithLaunchContext,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
};
use reth_provider::providers::BlockchainProvider;

use crate::node::BaseNode;

/// Alias for the Base node type adapter used by the runner.
pub type BaseNodeTypes<E = ()> = FullNodeTypesAdapter<BaseNode<E>, DatabaseEnv, BaseProvider<E>>;
/// Internal alias for the Base node components builder (default payload service).
pub type BaseComponentsBuilder<E = ()> = <BaseNode<E> as Node<BaseNodeTypes<E>>>::ComponentsBuilder;
/// Alias for the explicit components builder produced by [`BaseNode::components`].
pub type BaseNodeComponentsBuilder<Node, E = ()> = ComponentsBuilder<
    Node,
    BasePoolBuilder<BasePooledTransaction<BaseTransactionSigned, ConsensusPooledTransaction, E>>,
    BasicPayloadServiceBuilder<BasePayloadBuilder>,
    BaseNetworkBuilder,
    BaseExecutorBuilder,
    BaseConsensusBuilder,
>;
/// Internal alias for the Base node add-ons (all generics resolved).
pub(crate) type ConcreteBaseAddOns<E = ()> = <BaseNode<E> as Node<BaseNodeTypes<E>>>::AddOns;

/// A [`BlockchainProvider`] instance.
pub type BaseProvider<E = ()> =
    BlockchainProvider<NodeTypesWithDBAdapter<BaseNode<E>, DatabaseEnv>>;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = WithLaunchContext<NodeBuilder<DatabaseEnv, BaseChainSpec>>;
