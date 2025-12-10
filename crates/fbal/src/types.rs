use alloy_eip7928::AccountChanges;
use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Encodable, RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Default, Deserialize, Serialize, RlpEncodable, RlpDecodable)]
/// FlashblockAccessList represents the access list for a single flashblock
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
    /// Merge account changes from a new transaction into the access list
    pub fn merge_account_changes(&mut self, extension: Vec<AccountChanges>) {
        for new in extension.iter() {
            let mut found = false;
            for old in self.account_changes.iter_mut() {
                if old.address() == new.address() {
                    old.storage_changes.extend(new.storage_changes.clone());
                    old.storage_reads.extend(new.storage_reads.clone());
                    old.balance_changes.extend(new.balance_changes.clone());
                    old.nonce_changes.extend(new.nonce_changes.clone());
                    old.code_changes.extend(new.code_changes.clone());
                    found = true;
                    break;
                }
            }

            if (!found) {
                self.account_changes.push(new.clone());
            }
        }
    }

    /// Finalize the access list by computing the hash of the RLP-encoded account changes list
    pub fn finalize(&mut self) {
        let mut encoded = Vec::new();
        self.account_changes.encode(&mut encoded);
        self.fal_hash = keccak256(encoded);
    }
}
