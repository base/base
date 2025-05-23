use alloy_primitives::Bytes;
use alloy_rpc_types_eth::erc4337::TransactionConditional;
use serde::{Deserialize, Serialize};

pub const MAX_BLOCK_RANGE_BLOCKS: u64 = 10;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Bundle {
    #[serde(rename = "txs")]
    pub transactions: Vec<Bytes>,

    #[serde(rename = "maxBlockNumber")]
    pub block_number_max: Option<u64>,
}

impl Bundle {
    pub fn conditional(&self) -> TransactionConditional {
        TransactionConditional {
            block_number_min: None,
            block_number_max: self.block_number_max,
            known_accounts: Default::default(),
            timestamp_max: None,
            timestamp_min: None,
        }
    }
}
