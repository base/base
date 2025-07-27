/*

State
    - new_flashblock_received(...)
        - update
    - new_canonical_block_received(...)
        - clear
    - subscribe_to_flashblocks(...)
        - return a thing that fires on new one received and processed

Cache
    - current block number
    - pending block
    - map<hash => receipt>
    - map<hash => txn>
    - map<address => balance>
    - map<address => txn count>

    new_flashblock_received(...)
        - appends data to maps

 */
use crate::subscription::Flashblock;
use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, U256};

#[derive(Debug, Clone)]
pub struct PendingBlock {
    pub block_number: u64,
    pub index_number: u64,

    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
}

impl PendingBlock {
    pub fn empty() -> Self {
        Self {
            block_number: 0,
            index_number: 0,
            account_balances: HashMap::default(),
            transaction_count: HashMap::default(),
        }
    }
    pub fn new_block(flashblock: Flashblock) -> Self {
        let mut result = Self {
            block_number: flashblock.metadata.block_number,
            index_number: flashblock.index,
            account_balances: HashMap::default(),
            transaction_count: HashMap::default(),
        };
        result.insert_data(flashblock);
        result
    }

    pub fn extend_block(previous_cache: &PendingBlock, flashblock: Flashblock) -> Self {
        let mut result = previous_cache.clone();
        result.insert_data(flashblock);
        result
    }

    fn insert_data(&mut self, flashblock: Flashblock) {
        for (address, balance) in flashblock.metadata.new_account_balances {
            self.account_balances.insert(address, balance);
        }
    }

    pub fn get_transaction_count(&self, address: Address) -> U256 {
        self.transaction_count.get(&address).cloned().unwrap_or(U256::from(0))
    }

    pub fn get_balance(&self, address: Address) -> Option<U256> {
        self.account_balances.get(&address).cloned()
    }
}

#[cfg(test)]
mod tests {
    use crate::pending::PendingBlock;
    use crate::subscription::{Flashblock, Metadata};
    use alloy_primitives::{Address, Bloom, B256, B64, U256};
    use alloy_rpc_types_engine::PayloadId;
    use rollup_boost::ExecutionPayloadFlashblockDeltaV1;
    use std::collections::HashMap;

    fn default_fb() -> Flashblock {
        Flashblock {
            payload_id: PayloadId(B64::random()),
            index: 0,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::random(),
                receipts_root: B256::random(),
                logs_bloom: Bloom::random(),
                gas_used: 1000,
                block_hash: B256::random(),
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::random(),
            },
            metadata: Metadata {
                receipts: HashMap::default(),
                new_account_balances: HashMap::default(),
                block_number: 10,
            },
        }
    }

    #[test]
    fn test_get_balance() {
        let alice = Address::random();
        let bob = Address::random();

        let mut fb1 = default_fb();
        fb1.metadata
            .new_account_balances
            .insert(alice, U256::from(1000));

        let cache = PendingBlock::new_block(fb1);
        assert_eq!(
            cache.get_balance(alice).expect("should be set"),
            U256::from(1000)
        );
        assert!(cache.get_balance(bob).is_none());

        let mut fb2 = default_fb();
        fb2.metadata
            .new_account_balances
            .insert(alice, U256::from(2000));
        fb2.metadata
            .new_account_balances
            .insert(bob, U256::from(1000));

        let cache = PendingBlock::extend_block(&cache, fb2);
        assert_eq!(
            cache.get_balance(alice).expect("should be set"),
            U256::from(2000)
        );
        assert_eq!(
            cache.get_balance(bob).expect("should be set"),
            U256::from(1000)
        );
    }
}
