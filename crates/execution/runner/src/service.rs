//! Trait for customizing the payload service used by the node.

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
pub trait PayloadServiceBuilder: Send + 'static {
    /// The component builder type this produces.
    type ComponentsBuilder: NodeComponentsBuilder<
            BaseNodeTypes,
            Components = <BaseComponentsBuilder as NodeComponentsBuilder<BaseNodeTypes>>::Components,
        >;

    /// Build components using the given [`BaseNode`] configuration.
    fn build_components(self, base_node: &BaseNode) -> Self::ComponentsBuilder;
}

/// Default payload service using the standard Base payload builder.
#[derive(Debug, Default)]
pub struct DefaultPayloadServiceBuilder;

impl PayloadServiceBuilder for DefaultPayloadServiceBuilder {
    type ComponentsBuilder = ComponentsBuilder<
        BaseNodeTypes,
        BasePoolBuilder,
        BasicPayloadServiceBuilder<BasePayloadBuilder>,
        BaseNetworkBuilder,
        BaseExecutorBuilder,
        BaseConsensusBuilder,
    >;

    fn build_components(self, base_node: &BaseNode) -> Self::ComponentsBuilder {
        base_node.components()
    }
}
