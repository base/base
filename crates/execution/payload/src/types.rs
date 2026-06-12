use base_common_consensus::BasePrimitives;
use base_common_rpc_types_engine::TracedExecutionData;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{Block, NodePrimitives, SealedBlock};

use crate::{BaseBuiltPayload, TracedBasePayloadBuilderAttributes};

/// ZST that aggregates Base [`PayloadTypes`].
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct BasePayloadTypes<N: NodePrimitives = BasePrimitives>(core::marker::PhantomData<N>);

impl<N: NodePrimitives> PayloadTypes for BasePayloadTypes<N>
where
    BaseBuiltPayload<N>: BuiltPayload,
{
    type ExecutionData = TracedExecutionData;
    type BuiltPayload = BaseBuiltPayload<N>;
    type PayloadAttributes = TracedBasePayloadBuilderAttributes<N::SignedTx>;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        TracedExecutionData::from(
            base_common_rpc_types_engine::ExecutionData::from_block_unchecked(
                block.hash(),
                &block.into_block().into_ethereum_block(),
            ),
        )
    }
}
