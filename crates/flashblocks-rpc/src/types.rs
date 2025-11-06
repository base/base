use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use eyre::Context;
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Metadata {
    pub receipts: HashMap<B256, OpReceipt>,
    pub new_account_balances: HashMap<Address, U256>,
    pub block_number: u64,
}

#[derive(Debug, Clone)]
pub struct Flashblock {
    pub payload_id: PayloadId,
    pub index: u64,
    pub base: Option<ExecutionPayloadBaseV1>,
    pub diff: ExecutionPayloadFlashblockDeltaV1,
    pub metadata: Metadata,
}

impl TryFrom<FlashblocksPayloadV1> for Flashblock {
    type Error = eyre::Report;

    fn try_from(payload: FlashblocksPayloadV1) -> Result<Self, Self::Error> {
        let metadata: Metadata = serde_json::from_value(payload.metadata)
            .context("failed to parse flashblock metadata")?;

        Ok(Self {
            payload_id: payload.payload_id,
            index: payload.index,
            base: payload.base,
            diff: payload.diff,
            metadata,
        })
    }
}

pub trait FlashblocksReceiver {
    fn on_flashblock_received(&self, flashblock: Flashblock);
}
