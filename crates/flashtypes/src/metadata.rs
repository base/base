//! Contains the [`Metadata`] type used in Flashblocks.

use alloy_primitives::{Address, B256, U256, map::foldhash::HashMap};
use reth_optimism_primitives::OpReceipt;
use serde::{Deserialize, Serialize};

/// Metadata associated with a flashblock.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct Metadata {
    /// Transaction receipts indexed by hash.
    pub receipts: HashMap<B256, OpReceipt>,
    /// Updated account balances.
    pub new_account_balances: HashMap<Address, U256>,
    /// Block number this flashblock belongs to.
    pub block_number: u64,
}

impl Metadata {
    /// Returns true if there are no receipts or balance updates.
    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty() && self.new_account_balances.is_empty()
    }

    /// Returns the number of receipts tracked by this metadata.
    pub fn receipts_len(&self) -> usize {
        self.receipts.len()
    }

    /// Returns the number of balance updates in this metadata.
    pub fn balance_updates_len(&self) -> usize {
        self.new_account_balances.len()
    }

    /// Fetches the updated balance for a given address, if it exists.
    pub fn balance_for(&self, address: &Address) -> Option<&U256> {
        self.new_account_balances.get(address)
    }

    /// Returns true if there is a balance update for the provided address.
    pub fn has_balance_update(&self, address: &Address) -> bool {
        self.new_account_balances.contains_key(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_empty_metadata() {
        let metadata = Metadata::default();

        assert!(metadata.is_empty());
        assert_eq!(metadata.receipts_len(), 0);
        assert_eq!(metadata.balance_updates_len(), 0);
    }

    #[test]
    fn reports_balance_updates() {
        let mut balances = HashMap::default();
        let address = Address::from([0x11u8; 20]);
        balances.insert(address, U256::from(42u64));

        let metadata = Metadata {
            receipts: HashMap::default(),
            new_account_balances: balances,
            block_number: 1,
        };

        assert!(!metadata.is_empty());
        assert_eq!(metadata.balance_updates_len(), 1);
        assert!(metadata.has_balance_update(&address));
        assert_eq!(metadata.balance_for(&address), Some(&U256::from(42u64)));
    }
}
