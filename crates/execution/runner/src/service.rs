//! Trait for customizing the payload service used by the node.

use std::fmt;

use base_common_consensus::{
    BasePooledTransaction as ConsensusPooledTransaction, BaseTransactionSigned,
};
use base_execution_txpool::BasePooledTransaction;
use base_node_core::{
    BaseConsensusBuilder, BaseExecutorBuilder, BaseNetworkBuilder,
    node::{BasePayloadBuilder, BasePoolBuilder},
};
use reth_node_builder::{
    NodeComponentsBuilder,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
};

use crate::{
    node::BaseNode,
    types::{BaseComponentsBuilder, BaseNodeTypes},
};

/// Trait for customizing the payload service used by the node.
///
/// Implementors provide a custom [`NodeComponentsBuilder`] that wires in their
/// payload service. The default implementation uses reth's standard Base payload builder.
///
/// The produced components must have the same concrete `Components` type as the default
/// so that hooks (RPC, `ExEx`, node-started) remain type-compatible.
pub trait PayloadServiceBuilder<E = ()>: Send + 'static
where
    E: fmt::Debug + Clone + Send + Sync + Unpin + 'static,
{
    /// The component builder type this produces.
    type ComponentsBuilder: NodeComponentsBuilder<
            BaseNodeTypes<E>,
            Components = <BaseComponentsBuilder<E> as NodeComponentsBuilder<BaseNodeTypes<E>>>::Components,
        >;

    /// Build components using the given [`BaseNode`] configuration.
    fn build_components(self, base_node: &BaseNode<E>) -> Self::ComponentsBuilder;
}

/// Default payload service using the standard Base payload builder.
#[derive(Debug, Default)]
pub struct DefaultPayloadServiceBuilder;

impl<E> PayloadServiceBuilder<E> for DefaultPayloadServiceBuilder
where
    E: fmt::Debug + Clone + Send + Sync + Unpin + 'static,
{
    type ComponentsBuilder = ComponentsBuilder<
        BaseNodeTypes<E>,
        BasePoolBuilder<BasePooledTransaction<BaseTransactionSigned, ConsensusPooledTransaction, E>>,
        BasicPayloadServiceBuilder<BasePayloadBuilder>,
        BaseNetworkBuilder,
        BaseExecutorBuilder,
        BaseConsensusBuilder,
    >;

    fn build_components(self, base_node: &BaseNode<E>) -> Self::ComponentsBuilder {
        base_node.components()
    }
}
