use alloy_eip7928::AccountChanges;
use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Encodable, RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Debug, PartialEq, Eq, Default, Deserialize, Serialize, RlpEncodable, RlpDecodable,
)]
/// `FlashblockAccessList` represents the access list for a single flashblock
pub struct FlashblockAccessList {
    /// All the account changes in this access list
    pub account_changes: Vec<AccountChanges>,
    /// Minimum txn index from the full block that's included in this access list
    pub min_tx_index: u64,
    /// Maximum txn index from the full block that's included in this access list
    pub max_tx_index: u64,
    /// keccak256 hash of the RLP-encoded account changes list
    pub fal_hash: B256,
}

impl FlashblockAccessList {
    /// Builds a new `FlashblockAccessList` from the given account changes
    pub fn build(
        account_changes: Vec<AccountChanges>,
        min_tx_index: u64,
        max_tx_index: u64,
    ) -> Self {
        let mut encoded = Vec::new();
        account_changes.encode(&mut encoded);

        Self { account_changes, min_tx_index, max_tx_index, fal_hash: keccak256(encoded) }
    }
}
