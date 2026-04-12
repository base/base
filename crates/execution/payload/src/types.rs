use base_alloy_consensus::BasePrimitives;
use base_alloy_rpc_types_engine::{BasePayloadAttributes, ExecutionData};
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{Block, NodePrimitives, SealedBlock};

use crate::{OpBuiltPayload, OpPayloadBuilderAttributes};

/// ZST that aggregates Base [`PayloadTypes`].
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct BasePayloadTypes<N: NodePrimitives = BasePrimitives>(core::marker::PhantomData<N>);

impl<N: NodePrimitives> PayloadTypes for BasePayloadTypes<N>
where
    OpBuiltPayload<N>: BuiltPayload,
{
    type ExecutionData = ExecutionData;
    type BuiltPayload = OpBuiltPayload<N>;
    type PayloadAttributes = BasePayloadAttributes;
    type PayloadBuilderAttributes = OpPayloadBuilderAttributes<N::SignedTx>;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        ExecutionData::from_block_unchecked(block.hash(), &block.into_block().into_ethereum_block())
    }
}
