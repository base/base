use alloy_eip7928::{
    AccountChanges, BalanceChange, CodeChange, NonceChange, SlotChanges, StorageChange,
};
use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Default, Deserialize, Serialize)]
pub struct FlashblockAccessList {
    pub account_changes: Vec<AccountChanges>,
    pub min_tx_index: u64,
    pub max_tx_index: u64,
    pub fal_hash: B256,
}
