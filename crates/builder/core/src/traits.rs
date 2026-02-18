/// Trait bounds for OP Stack builder node types.
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::OpEngineTypes;
use reth_optimism_primitives::OpPrimitives;

/// Trait alias bounding a full node to OP Stack types.
pub trait NodeBounds:
    FullNodeTypes<
    Types: NodeTypes<Payload = OpEngineTypes, ChainSpec = OpChainSpec, Primitives = OpPrimitives>,
>
{
}

impl<T> NodeBounds for T where
    T: FullNodeTypes<
        Types: NodeTypes<
            Payload = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
        >,
    >
{
}
