use base_alloy_consensus::OpPrimitives;
use base_alloy_rpc_types_engine::OpExecutionData;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{Block, NodePrimitives, SealedBlock};

use crate::{OpBuiltPayload, OpPayloadAttributes, OpPayloadBuilderAttributes};

/// ZST that aggregates Base [`PayloadTypes`].
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct OpPayloadTypes<N: NodePrimitives = OpPrimitives>(core::marker::PhantomData<N>);

impl<N: NodePrimitives> PayloadTypes for OpPayloadTypes<N>
where
    OpBuiltPayload<N>: BuiltPayload,
{
    type ExecutionData = OpExecutionData;
    type BuiltPayload = OpBuiltPayload<N>;
    type PayloadAttributes = OpPayloadAttributes;
    type PayloadBuilderAttributes = OpPayloadBuilderAttributes<N::SignedTx>;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        OpExecutionData::from_block_unchecked(
            block.hash(),
            &block.into_block().into_ethereum_block(),
        )
    }
}
