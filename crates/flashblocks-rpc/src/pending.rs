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
use alloy_consensus::transaction::SignerRecoverable;
use crate::subscription::Flashblock;
use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, U256};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use reth_optimism_primitives::OpBlock;
use rollup_boost::ExecutionPayloadBaseV1;

#[derive(Debug, Clone)]
pub struct PendingBlock {
    pub block_number: u64,
    pub index_number: u64,

    base: ExecutionPayloadBaseV1,
    flashblocks: Vec<Flashblock>,

    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
}

impl PendingBlock {
    pub fn empty() -> Self {
        Self {
            block_number: 0,
            index_number: 0,
            base: ExecutionPayloadBaseV1::default(),
            flashblocks: vec![],
            account_balances: HashMap::default(),
            transaction_count: HashMap::default(),
        }
    }
    pub fn new_block(flashblock: Flashblock) -> Self {
        let mut result = Self {
            block_number: flashblock.metadata.block_number,
            index_number: flashblock.index,
            base: flashblock.base.clone().unwrap(), //todo!
            flashblocks: vec![],
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
        self.flashblocks.push(flashblock.clone());

        let transactions = self.flashblocks.iter()
            .map(|flashblock| flashblock.diff.transactions.clone())
            .flatten()
            .collect();

        let withdrawals = self.flashblocks.iter()
            .map(|flashblock| flashblock.diff.withdrawals.clone())
            .flatten()
            .collect();

        let execution_payload: ExecutionPayloadV3 = ExecutionPayloadV3 {
            blob_gas_used: 0,
            excess_blob_gas: 0,
            payload_inner: ExecutionPayloadV2 {
                withdrawals,
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: self.base.parent_hash,
                    fee_recipient: self.base.fee_recipient,
                    state_root: flashblock.diff.state_root,
                    receipts_root: flashblock.diff.receipts_root,
                    logs_bloom: flashblock.diff.logs_bloom,
                    prev_randao: self.base.prev_randao,
                    block_number: self.base.block_number,
                    gas_limit: self.base.gas_limit,
                    gas_used: flashblock.diff.gas_used,
                    timestamp: self.base.timestamp,
                    extra_data: self.base.extra_data.clone(),
                    base_fee_per_gas: self.base.base_fee_per_gas,
                    block_hash: flashblock.diff.block_hash,
                    transactions,
                },
            },
        };

        // TODO: Error case
        let block: OpBlock = execution_payload.try_into_block().unwrap();

        // Redo transaction count for the whole block.
        self.transaction_count.clear();
        for transaction in block.body.transactions.iter() {
            let signer = match transaction.recover_signer() {
                Ok(signer) => signer,
                Err(_err) => panic!("hello world todo"),
            };

            let zero = U256::from(0);
            let current_count = self.transaction_count.get(&signer)
                .unwrap_or(&zero);

            _ = self.transaction_count.insert(
                signer,
                *current_count + U256::from(1),
            )
        }

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
    use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
    use std::collections::HashMap;

    fn default_fb() -> Flashblock {
        Flashblock {
            payload_id: PayloadId(B64::random()),
            index: 0,
            base: Some(ExecutionPayloadBaseV1::default()),
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
