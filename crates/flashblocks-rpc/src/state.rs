use crate::metrics::Metrics;
use crate::pending::PendingBlock;
use crate::subscription::Flashblock;
use alloy_primitives::{Address, TxHash, U256};
use arc_swap::ArcSwap;
use op_alloy_network::Optimism;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::OpReceipt;
use reth_rpc_convert::RpcTransaction;
use reth_rpc_eth_api::{RpcBlock, RpcReceipt};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use tracing::info;

/// A receipt with its transaction hash for broadcasting
#[derive(Debug, Clone)]
pub struct ReceiptWithHash {
    pub tx_hash: TxHash,
    pub receipt: OpReceipt,
    pub block_number: u64,
}

#[derive(Debug, Clone)]
pub struct FlashblocksState {
    current_state: Arc<ArcSwap<PendingBlock>>,

    receipt_sender: broadcast::Sender<ReceiptWithHash>,
    metrics: Metrics,
    chain_spec: Arc<OpChainSpec>,
}

impl FlashblocksState {
    pub fn new(chain_spec: Arc<OpChainSpec>, receipt_buffer_size: usize) -> Self {
        Self {
            current_state: Arc::new(ArcSwap::from_pointee(PendingBlock::empty(
                chain_spec.clone(),
            ))),
            chain_spec,
            receipt_sender: broadcast::channel(receipt_buffer_size).0,
            metrics: Metrics::default(),
        }
    }

    pub fn block_by_number(&self, full: bool) -> Option<RpcBlock<Optimism>> {
        // todo bug, when no pending state should be global on this class, not per PendingBlock
        self.current_state.load().get_block(full)
    }

    pub fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        self.current_state.load().get_receipt(tx_hash)
    }

    pub fn get_transaction_count(&self, address: Address) -> U256 {
        self.current_state.load().get_transaction_count(address)
    }

    pub fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<RpcTransaction<Optimism>> {
        self.current_state.load().get_transaction_by_hash(tx_hash)
    }

    pub fn get_balance(&self, address: Address) -> Option<U256> {
        self.current_state.load().get_balance(address)
    }

    pub fn subscribe_to_receipts(&self) -> broadcast::Receiver<ReceiptWithHash> {
        self.receipt_sender.subscribe()
    }

    fn is_next_flashblock(&self, flashblock: &Flashblock) -> bool {
        flashblock.metadata.block_number == self.current_state.load().block_number
            && flashblock.index == self.current_state.load().index_number + 1
    }

    pub fn on_flashblock_received(&self, flashblock: Flashblock) {
        let msg_processing_start_time = Instant::now();
        let current_state = self.current_state.load();

        if flashblock.index == 0 {
            info!(
                message = "block processing completed",
                processing_time = ?msg_processing_start_time.elapsed()
            );

            self.metrics
                .flashblocks_in_block
                .record((current_state.index_number + 1) as f64);

            self.current_state.swap(Arc::new(PendingBlock::new_block(
                self.chain_spec.clone(),
                flashblock,
            )));
        } else if self.is_next_flashblock(&flashblock) {
            self.current_state.swap(Arc::new(PendingBlock::extend_block(
                &current_state,
                flashblock,
            )));
        } else if current_state.block_number != flashblock.metadata.block_number {
            info!(
                message = "Received Flashblock for new block, zero'ing Flashblocks until ",
                curr_block = %current_state.block_number,
                new_block = %flashblock.metadata.block_number,
            );

            self.current_state
                .swap(Arc::new(PendingBlock::empty(self.chain_spec.clone())));
        } else {
            info!(
                message = "None sequential Flashblocks, keeping cache",
                curr_block = %current_state.block_number,
                new_block = %flashblock.metadata.block_number,
            );
        }

        // TODO!! publish Flashblock
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::Metadata;
    use alloy_consensus::Receipt;
    use alloy_primitives::map::foldhash::HashMap;
    use alloy_primitives::{Address, Bloom, Bytes, B256, B64, U256};
    use alloy_rpc_types_engine::PayloadId;
    use op_alloy_consensus::OpDepositReceipt;
    use reth_optimism_chainspec::OpChainSpecBuilder;
    use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
    use std::str::FromStr;

    fn new_state() -> FlashblocksState {
        let chain_spec = Arc::new(
            OpChainSpecBuilder::base_mainnet()
                .ecotone_activated()
                .build(),
        );

        FlashblocksState::new(chain_spec, 2000)
    }

    fn create_first_payload() -> Flashblock {
        let block_info_tx = Bytes::from_str(
            "0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000")
            .unwrap();

        let block_info_txn_hash =
            B256::from_str("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6")
                .unwrap();

        // First payload (index 0) setup remains the same
        let base = ExecutionPayloadBaseV1 {
            parent_hash: Default::default(),
            parent_beacon_block_root: Default::default(),
            fee_recipient: Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            block_number: 1,
            gas_limit: 1000000,
            timestamp: 1234567890,
            prev_randao: Default::default(),
            extra_data: Default::default(),
            base_fee_per_gas: U256::from(10000),
        };

        let delta = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![block_info_tx],
            withdrawals: vec![],
            state_root: Default::default(),
            receipts_root: Default::default(),
            logs_bloom: Default::default(),
            gas_used: 0,
            block_hash: Default::default(),
            withdrawals_root: Default::default(),
        };

        let metadata = Metadata {
            block_number: 1,
            receipts: {
                let mut receipts = alloy_primitives::map::HashMap::default();
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
        };

        Flashblock {
            index: 0,
            payload_id: PayloadId::new([0; 8]),
            base: Some(base),
            diff: delta,
            metadata: metadata,
        }
    }

    fn create_second_payload() -> Flashblock {
        // Create second payload (index 1) with transactions
        // tx1 hash: 0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c
        // tx2 hash: 0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8
        let tx1 = Bytes::from_str("0x02f87483014a3482017e8459682f0084596830a98301f1d094b01866f195533de16eb929b73f87280693ca0cb480844e71d92dc001a0a658c18bdba29dd4022ee6640fdd143691230c12b3c8c86cf5c1a1f1682cc1e2a0248a28763541ebed2b87ecea63a7024b5c2b7de58539fa64c887b08f5faf29c1").unwrap();
        let tx2 = Bytes::from_str("0xf8cd82016d8316e5708302c01c94f39635f2adf40608255779ff742afe13de31f57780b8646e530e9700000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000001bc16d674ec8000000000000000000000000000000000000000000000000000156ddc81eed2a36d68302948ba0a608703e79b22164f74523d188a11f81c25a65dd59535bab1cd1d8b30d115f3ea07f4cfbbad77a139c9209d3bded89091867ff6b548dd714109c61d1f8e7a84d14").unwrap();

        let delta2 = ExecutionPayloadFlashblockDeltaV1 {
            transactions: vec![tx1.clone(), tx2.clone()],
            withdrawals: vec![],
            state_root: B256::repeat_byte(0x1),
            receipts_root: B256::repeat_byte(0x2),
            logs_bloom: Default::default(),
            gas_used: 21000,
            block_hash: B256::repeat_byte(0x3),
            withdrawals_root: Default::default(),
        };

        let metadata2 = Metadata {
            block_number: 1,
            receipts: {
                let mut receipts = HashMap::default();
                receipts.insert(
                    B256::from_str(
                        "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c",
                    )
                    .unwrap(),
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 21000,
                        logs: vec![],
                    }),
                );
                receipts.insert(
                    B256::from_str(
                        "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8",
                    )
                    .unwrap(),
                    OpReceipt::Legacy(Receipt {
                        status: true.into(),
                        cumulative_gas_used: 42000,
                        logs: vec![],
                    }),
                );
                receipts
            },
            new_account_balances: {
                let mut map = HashMap::default();
                map.insert(
                    Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
                    U256::from_str("0x1234").unwrap(),
                );
                map
            },
        };

        Flashblock {
            index: 1,
            payload_id: PayloadId::new([0; 8]),
            base: None,
            diff: delta2,
            metadata: metadata2,
        }
    }

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

    #[test]
    fn test_process_payload() {
        let cache = new_state();
        // let mut receipt_receiver = cache.subscribe_to_receipts();

        let payload = create_first_payload();

        // Process first payload
        cache.on_flashblock_received(payload);

        let payload2 = create_second_payload();
        // Process second payload
        cache.on_flashblock_received(payload2);

        // Verify final state
        // TODO!
        // let final_block = cache.get::<OpBlock>(&CacheKey::PendingBlock).unwrap();
        // assert_eq!(final_block.body.transactions.len(), 3);
        // assert_eq!(final_block.header.state_root, B256::repeat_byte(0x1));
        // assert_eq!(final_block.header.receipts_root, B256::repeat_byte(0x2));
        // assert_eq!(final_block.header.gas_used, 21000);
        // assert_eq!(final_block.header.base_fee_per_gas, Some(10000));
        //
        // let tx1_receipt = cache
        //     .get::<OpReceipt>(&CacheKey::Receipt(
        //         B256::from_str(
        //             "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c",
        //         )
        //         .unwrap(),
        //     ))
        //     .unwrap();
        // assert_eq!(tx1_receipt.cumulative_gas_used(), 21000);
        //
        // let tx2_receipt = cache
        //     .get::<OpReceipt>(&CacheKey::Receipt(
        //         B256::from_str(
        //             "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8",
        //         )
        //         .unwrap(),
        //     ))
        //     .unwrap();
        // assert_eq!(tx2_receipt.cumulative_gas_used(), 42000);
        //
        // // verify tx_sender, tx_block_number, tx_idx
        // let tx_sender = cache
        //     .get::<Address>(&CacheKey::TransactionSender(
        //         B256::from_str(
        //             "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c",
        //         )
        //         .unwrap(),
        //     ))
        //     .unwrap();
        // assert_eq!(
        //     tx_sender,
        //     Address::from_str("0xb63d5fd2e6c53fe06680c47736aba771211105e4").unwrap()
        // );
        //
        // let tx_block_number = cache
        //     .get::<u64>(&CacheKey::TransactionBlockNumber(
        //         B256::from_str(
        //             "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c",
        //         )
        //         .unwrap(),
        //     ))
        //     .unwrap();
        // assert_eq!(tx_block_number, 1);
        //
        // let tx_idx = cache
        //     .get::<u64>(&CacheKey::TransactionIndex(
        //         B256::from_str(
        //             "0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c",
        //         )
        //         .unwrap(),
        //     ))
        //     .unwrap();
        // assert_eq!(tx_idx, 1);
        //
        // let tx_sender2 = cache
        //     .get::<Address>(&CacheKey::TransactionSender(
        //         B256::from_str(
        //             "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8",
        //         )
        //         .unwrap(),
        //     ))
        //     .unwrap();
        // assert_eq!(
        //     tx_sender2,
        //     Address::from_str("0x6e5e56b972374e4fde8390df0033397df931a49d").unwrap()
        // );
        //
        // let tx_block_number2 = cache
        //     .get::<u64>(&CacheKey::TransactionBlockNumber(
        //         B256::from_str(
        //             "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8",
        //         )
        //         .unwrap(),
        //     ))
        //     .unwrap();
        // assert_eq!(tx_block_number2, 1);
        //
        // let tx_idx2 = cache
        //     .get::<u64>(&CacheKey::TransactionIndex(
        //         B256::from_str(
        //             "0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8",
        //         )
        //         .unwrap(),
        //     ))
        //     .unwrap();
        // assert_eq!(tx_idx2, 2);
    }

    // WHY??
    // #[test]
    // fn test_skip_initial_non_zero_index_payload() {
    //     let cache = new_state();
    //     let metadata = Metadata {
    //         block_number: 1,
    //         receipts: HashMap::default(),
    //         new_account_balances: HashMap::default(),
    //     };
    //
    //     let payload = Flashblock {
    //         payload_id: PayloadId::new([0; 8]),
    //         index: 1, // Non-zero index but no base in cache
    //         base: None,
    //         diff: ExecutionPayloadFlashblockDeltaV1::default(),
    //         metadata,
    //     };
    //
    //     // Process payload
    //     cache.on_flashblock_received(payload);
    //
    //     // Verify no block was stored, since it skips the first payload
    //     assert!(cache.get::<OpBlock>(&CacheKey::PendingBlock).is_none());
    // }
}
