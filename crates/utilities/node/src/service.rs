//! Trait for customizing the payload service used by the node.

use base_execution_node::{
    OpConsensusBuilder, OpExecutorBuilder, OpNetworkBuilder,
    node::{OpPayloadBuilder, OpPoolBuilder},
};
use reth_node_builder::{
    NodeComponentsBuilder,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
};

use crate::{
    node::BaseNode,
    types::{OpComponentsBuilder, OpNodeTypes},
};

/// Trait for customizing the payload service used by the node.
///
/// Implementors provide a custom [`NodeComponentsBuilder`] that wires in their
/// payload service. The default implementation uses reth's standard OP payload builder.
///
/// The produced components must have the same concrete `Components` type as the default
/// so that hooks (RPC, `ExEx`, node-started) remain type-compatible.
pub trait PayloadServiceBuilder: Send + 'static {
    /// The component builder type this produces.
    type ComponentsBuilder: NodeComponentsBuilder<
            OpNodeTypes,
            Components = <OpComponentsBuilder as NodeComponentsBuilder<OpNodeTypes>>::Components,
        >;

    /// Build components using the given [`BaseNode`] configuration.
    fn build_components(self, base_node: &BaseNode) -> Self::ComponentsBuilder;
}

/// Default payload service using the standard OP payload builder.
#[derive(Debug, Default)]
pub struct DefaultPayloadServiceBuilder;

impl PayloadServiceBuilder for DefaultPayloadServiceBuilder {
    type ComponentsBuilder = ComponentsBuilder<
        OpNodeTypes,
        OpPoolBuilder,
        BasicPayloadServiceBuilder<OpPayloadBuilder>,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >;

    fn build_components(self, base_node: &BaseNode) -> Self::ComponentsBuilder {
        base_node.components()
    }
}
