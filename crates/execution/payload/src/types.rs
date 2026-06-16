use alloy_primitives::Bytes;
use base_common_consensus::BasePrimitives;
use base_common_rpc_types_engine::{ExecutionData, TracedExecutionData};
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
        bal: Option<Bytes>,
    ) -> Self::ExecutionData {
        TracedExecutionData::from(ExecutionData::from_block_unchecked_with_extras(
            block.hash(),
            &block.into_block().into_ethereum_block(),
            bal,
        ))
    }
}

impl<N: NodePrimitives> From<BaseBuiltPayload<N>> for TracedExecutionData
where
    BaseBuiltPayload<N>: BuiltPayload,
    ExecutionData: From<BaseBuiltPayload<N>>,
{
    fn from(value: BaseBuiltPayload<N>) -> Self {
        Self::from(ExecutionData::from(value))
    }
}
