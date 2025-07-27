use crate::subscription::Flashblock;
use alloy_consensus::transaction::{Recovered, SignerRecoverable, TransactionMeta};
use alloy_consensus::TxReceipt;
use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, TxHash, B256, U256};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use eyre::eyre;
use op_alloy_rpc_types::OpTransactionReceipt;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::{OpBlock, OpPrimitives};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_rpc_convert::transaction::ConvertReceiptInput;
use rollup_boost::ExecutionPayloadBaseV1;
use std::borrow::Cow;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct PendingBlock {
    base: ExecutionPayloadBaseV1,
    flashblocks: Vec<Flashblock>,
    chain_spec: Arc<OpChainSpec>,

    pub block_number: u64,
    pub index_number: u64,
    account_balances: HashMap<Address, U256>,
    transaction_count: HashMap<Address, U256>,
    transaction_receipts: HashMap<B256, OpTransactionReceipt>,
}

impl PendingBlock {
    pub fn empty(chain_spec: Arc<OpChainSpec>) -> Self {
        Self {
            chain_spec,
            block_number: 0,
            index_number: 0,
            base: ExecutionPayloadBaseV1::default(),
            flashblocks: vec![],
            account_balances: HashMap::default(),
            transaction_count: HashMap::default(),
            transaction_receipts: HashMap::default(),
        }
    }
    pub fn new_block(chain_spec: Arc<OpChainSpec>, flashblock: Flashblock) -> Self {
        let mut result = Self {
            chain_spec,
            block_number: flashblock.metadata.block_number,
            index_number: flashblock.index,
            base: flashblock.base.clone().unwrap(), //todo!
            flashblocks: vec![],
            account_balances: HashMap::default(),
            transaction_count: HashMap::default(),
            transaction_receipts: HashMap::default(),
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

        let transactions = self
            .flashblocks
            .iter()
            .flat_map(|flashblock| flashblock.diff.transactions.clone())
            .collect();

        let withdrawals = self
            .flashblocks
            .iter()
            .flat_map(|flashblock| flashblock.diff.withdrawals.clone())
            .collect();

        let receipt_by_hash = self
            .flashblocks
            .iter()
            .map(|flashblock| flashblock.metadata.receipts.clone())
            .fold(HashMap::default(), |mut acc, receipts| {
                acc.extend(receipts);
                acc
            });

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

        // TODO: Error cases
        let block: OpBlock = execution_payload.try_into_block().unwrap();
        let mut l1_block_info = reth_optimism_evm::extract_l1_info(&block.body).unwrap();

        let block_hash = block.header.hash_slow();

        // TODO: Can we do this without zero'ing out the txn count for the whole block.
        self.transaction_count.clear();
        self.transaction_receipts.clear();
        let mut gas_used = 0;
        let mut next_log_index = 0;
        for (idx, transaction) in block.body.transactions.iter().enumerate() {
            let sender = match transaction.recover_signer() {
                Ok(signer) => signer,
                Err(_err) => panic!("hello world todo"),
            };

            let zero = U256::from(0);
            let current_count = self.transaction_count.get(&sender).unwrap_or(&zero);

            _ = self
                .transaction_count
                .insert(sender, *current_count + U256::from(1));

            // TODO
            let receipt = receipt_by_hash
                .get(&transaction.tx_hash())
                .cloned()
                .ok_or(eyre!("todo"))
                .unwrap();

            let meta = TransactionMeta {
                tx_hash: transaction.tx_hash(),
                index: idx as u64,
                block_hash,
                block_number: block.number,
                base_fee: block.base_fee_per_gas,
                excess_blob_gas: block.excess_blob_gas,
                timestamp: block.timestamp,
            };

            let input: ConvertReceiptInput<'_, OpPrimitives> = ConvertReceiptInput {
                receipt: Cow::Borrowed(&receipt),
                tx: Recovered::new_unchecked(transaction, sender),
                gas_used: receipt.cumulative_gas_used() - gas_used,
                next_log_index,
                meta,
            };

            let op_receipt =
                OpReceiptBuilder::new(self.chain_spec.as_ref(), input, &mut l1_block_info)
                    .expect("failed to build receipt")
                    .build();

            self.transaction_receipts
                .insert(transaction.tx_hash(), op_receipt);

            gas_used = receipt.cumulative_gas_used();
            next_log_index += receipt.logs().len();
        }

        for (address, balance) in flashblock.metadata.new_account_balances {
            self.account_balances.insert(address, balance);
        }
    }

    pub fn get_receipt(&self, tx_hash: TxHash) -> Option<OpTransactionReceipt> {
        self.transaction_receipts.get(&tx_hash).cloned()
    }

    pub fn get_transaction_count(&self, address: Address) -> U256 {
        self.transaction_count
            .get(&address)
            .cloned()
            .unwrap_or(U256::from(0))
    }

    pub fn get_balance(&self, address: Address) -> Option<U256> {
        self.account_balances.get(&address).cloned()
    }
}

#[cfg(test)]
mod tests {
    use crate::pending::PendingBlock;
    use crate::subscription::{Flashblock, Metadata};
    use alloy_consensus::Receipt;
    use alloy_primitives::{Address, Bloom, Bytes, B256, B64, U256};
    use alloy_rpc_types_engine::PayloadId;
    use op_alloy_consensus::OpDepositReceipt;
    use reth_optimism_chainspec::OpChainSpecBuilder;
    use reth_optimism_primitives::OpReceipt;
    use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::Arc;

    fn default_fb() -> Flashblock {
        let block_info_tx = Bytes::from_str(
            "0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000")
            .unwrap();

        let block_info_txn_hash =
            B256::from_str("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6")
                .unwrap();

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
                transactions: vec![block_info_tx],
                withdrawals: vec![],
                withdrawals_root: B256::random(),
            },
            metadata: Metadata {
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        block_info_txn_hash,
                        OpReceipt::Deposit(OpDepositReceipt {
                            inner: Receipt {
                                status: true.into(),
                                cumulative_gas_used: 10000,
                                logs: vec![],
                            },
                            deposit_nonce: Some(4012991u64),
                            deposit_receipt_version: None,
                        }),
                    );
                    receipts
                },
                new_account_balances: HashMap::default(),
                block_number: 10,
            },
        }
    }

    #[test]
    fn test_get_balance() {
        let cs = Arc::new(OpChainSpecBuilder::base_mainnet().build());

        let alice = Address::random();
        let bob = Address::random();

        let mut fb1 = default_fb();
        fb1.metadata
            .new_account_balances
            .insert(alice, U256::from(1000));

        let cache = PendingBlock::new_block(cs, fb1);
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
