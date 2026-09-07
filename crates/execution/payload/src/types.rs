use alloy_primitives::Bytes;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_rpc_types_engine::ExecutionData;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{Block, SealedBlock};

use crate::{BaseBuiltPayload, BasePayloadBuilderAttributes};

/// ZST that aggregates Base [`PayloadTypes`].
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct BasePayloadTypes;

impl PayloadTypes for BasePayloadTypes
where
    BaseBuiltPayload: BuiltPayload,
{
    type ExecutionData = ExecutionData;
    type BuiltPayload = BaseBuiltPayload;
    type PayloadAttributes = BasePayloadBuilderAttributes<BaseTxEnvelope>;

    fn block_to_payload(block: SealedBlock<BaseBlock>, bal: Option<Bytes>) -> Self::ExecutionData {
        ExecutionData::from_block_unchecked_with_extras(
            block.hash(),
            &block.into_block().into_ethereum_block(),
            bal,
        )
    }
}
