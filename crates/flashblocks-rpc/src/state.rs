use crate::metrics::Metrics;
use crate::pending::PendingBlock;
use crate::rpc::FlashblocksAPI;
use crate::subscription::Flashblock;
use alloy_primitives::{Address, TxHash, U256};
use arc_swap::ArcSwapOption;
use op_alloy_network::Optimism;
use reth_optimism_chainspec::OpChainSpec;
use reth_rpc_convert::RpcTransaction;
use reth_rpc_eth_api::{RpcBlock, RpcReceipt};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;
use tracing::{debug, info, warn};

// Buffer 4s of Flashblocks
const BUFFER_SIZE: usize = 20;

#[derive(Debug, Clone)]
pub struct FlashblocksState {
    pending_block: Arc<ArcSwapOption<PendingBlock>>,

    flashblock_sender: Sender<Flashblock>,
    metrics: Metrics,
    chain_spec: Arc<OpChainSpec>,
}

impl FlashblocksState {
    pub fn new(chain_spec: Arc<OpChainSpec>) -> Self {
        Self {
            pending_block: Arc::new(ArcSwapOption::new(None)),
            chain_spec,
            flashblock_sender: broadcast::channel(BUFFER_SIZE).0,
            metrics: Metrics::default(),
        }
    }

    fn is_next_flashblock(
        &self,
        pending_block: &Arc<PendingBlock>,
        flashblock: &Flashblock,
    ) -> bool {
        flashblock.metadata.block_number == pending_block.block_number
            && flashblock.index == pending_block.flashblock_idx + 1
    }

    fn update_block(&self, block: eyre::Result<PendingBlock>) {
        let start_time = Instant::now();
        match block {
            Ok(block) => {
                self.pending_block.swap(Some(Arc::new(block)));
                self.metrics
                    .block_processing_duration
                    .record(start_time.elapsed());
            }
            Err(e) => {
                warn!(message = "could not process Flashblock", error = %e);
                self.metrics.block_processing_error.increment(1);
            }
        }
    }

    pub fn on_flashblock_received(&self, flashblock: Flashblock) {
        match self.pending_block.load_full() {
            Some(pending_block) => {
                if flashblock.index == 0 {
                    self.metrics
                        .flashblocks_in_block
                        .record((pending_block.flashblock_idx + 1) as f64);

                    self.update_block(PendingBlock::new_block(
                        self.chain_spec.clone(),
                        flashblock.clone(),
                    ));
                } else if self.is_next_flashblock(&pending_block, &flashblock) {
                    self.update_block(PendingBlock::extend_block(
                        &pending_block,
                        flashblock.clone(),
                    ));
                } else if pending_block.block_number != flashblock.metadata.block_number {
                    self.metrics.unexpected_block_order.increment(1);
                    self.pending_block.swap(None);

                    info!(
                        message = "Received Flashblock for new block, zeroing Flashblocks until we receive a base Flashblock",
                        curr_block = %pending_block.block_number,
                        new_block = %flashblock.metadata.block_number,
                    );
                } else {
                    self.metrics.unexpected_block_order.increment(1);

                    info!(
                        message = "None sequential Flashblocks, keeping cache",
                        curr_block = %pending_block.block_number,
                        new_block = %flashblock.metadata.block_number,
                    );
                }
            }
            None => {
                if flashblock.index == 0 {
                    self.update_block(PendingBlock::new_block(
                        self.chain_spec.clone(),
                        flashblock.clone(),
                    ));
                } else {
                    debug!(message = "waiting for first Flashblock")
                }
            }
        }

        _ = self.flashblock_sender.send(flashblock);
    }
}

impl FlashblocksAPI for FlashblocksState {
    fn get_block(&self, full: bool) -> Option<RpcBlock<Optimism>> {
        self.pending_block.load_full().map(|pb| pb.get_block(full))
    }

    fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        self.pending_block
            .load_full()
            .and_then(|pb| pb.get_receipt(tx_hash))
    }

    fn get_transaction_count(&self, address: Address) -> U256 {
        self.pending_block
            .load_full()
            .map(|pb| pb.get_transaction_count(address))
            .unwrap_or_else(|| U256::from(0))
    }

    fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<RpcTransaction<Optimism>> {
        self.pending_block
            .load_full()
            .and_then(|pb| pb.get_transaction_by_hash(tx_hash))
    }

    fn get_balance(&self, address: Address) -> Option<U256> {
        self.pending_block
            .load_full()
            .and_then(|pb| pb.get_balance(address))
    }

    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Flashblock> {
        self.flashblock_sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::Metadata;
    use alloy_consensus::Receipt;
    use alloy_primitives::map::foldhash::HashMap;
    use alloy_primitives::{address, b256, bytes, Address, Bytes, B256, U256};
    use alloy_provider::network::ReceiptResponse;
    use alloy_rpc_types_engine::PayloadId;
    use op_alloy_consensus::OpDepositReceipt;
    use reth_optimism_chainspec::OpChainSpecBuilder;
    use reth_optimism_primitives::OpReceipt;
    use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
    use std::str::FromStr;

    const BLOCK_NUM: u64 = 100;

    const FEE_RECIPIENT: Address = address!("0x1234567890123456789012345678901234567890");
    // https://basescan.org/tx/0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6
    const BLOCK_INFO_SENDER: Address = address!("0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001");
    const BLOCK_INFO_HASH: B256 =
        b256!("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6");
    const BLOCK_INFO_TXN: Bytes = bytes!("0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000");

    // https://sepolia.basescan.org/tx/0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c
    const TX1_SENDER: Address = address!("0xb63d5Fd2e6c53fE06680c47736aba771211105e4");
    const TX1_HASH: B256 =
        b256!("0x3cbbc9a6811ac5b2a2e5780bdb67baffc04246a59f39e398be048f1b2d05460c");
    const TX1_DATA: Bytes = bytes!("0x02f87483014a3482017e8459682f0084596830a98301f1d094b01866f195533de16eb929b73f87280693ca0cb480844e71d92dc001a0a658c18bdba29dd4022ee6640fdd143691230c12b3c8c86cf5c1a1f1682cc1e2a0248a28763541ebed2b87ecea63a7024b5c2b7de58539fa64c887b08f5faf29c1");

    //https://sepolia.basescan.org/tx/0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8
    const TX2_SENDER: Address = address!("0x6E5e56b972374e4fDe8390dF0033397Df931A49D");
    const TX2_HASH: B256 =
        b256!("0xa6155b295085d3b87a3c86e342fe11c3b22f9952d0d85d9d34d223b7d6a17cd8");
    const TX2_DATA: Bytes = bytes!("0xf8cd82016d8316e5708302c01c94f39635f2adf40608255779ff742afe13de31f57780b8646e530e9700000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000001bc16d674ec8000000000000000000000000000000000000000000000000000156ddc81eed2a36d68302948ba0a608703e79b22164f74523d188a11f81c25a65dd59535bab1cd1d8b30d115f3ea07f4cfbbad77a139c9209d3bded89091867ff6b548dd714109c61d1f8e7a84d14");

    fn new_state() -> FlashblocksState {
        let chain_spec = Arc::new(
            OpChainSpecBuilder::base_mainnet()
                .ecotone_activated()
                .build(),
        );

        FlashblocksState::new(chain_spec)
    }

    fn create_first_flashblock(number: u64) -> Flashblock {
        Flashblock {
            index: 0,
            payload_id: PayloadId::new([0; 8]),
            base: Some(ExecutionPayloadBaseV1 {
                fee_recipient: FEE_RECIPIENT,
                block_number: number,
                gas_limit: 1000000,
                timestamp: 1234567890,
                base_fee_per_gas: U256::from(10000),
                ..Default::default()
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                transactions: vec![BLOCK_INFO_TXN],
                ..Default::default()
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        BLOCK_INFO_HASH,
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
            },
        }
    }

    fn create_second_flashblock() -> Flashblock {
        Flashblock {
            index: 1,
            base: None,
            payload_id: PayloadId::new([0; 8]),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                transactions: vec![TX1_DATA, TX2_DATA],
                withdrawals: vec![],
                state_root: B256::repeat_byte(0x1),
                receipts_root: B256::repeat_byte(0x2),
                logs_bloom: Default::default(),
                gas_used: 21000,
                block_hash: B256::repeat_byte(0x3),
                withdrawals_root: Default::default(),
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        TX1_HASH,
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 21000,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        TX2_HASH,
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
                    map.insert(FEE_RECIPIENT, U256::from_str("0x1234").unwrap());
                    map
                },
            },
        }
    }

    #[test]
    fn test_processing_error() {
        let state = new_state();

        assert!(state.get_block(true).is_none());

        state.on_flashblock_received(create_first_flashblock(BLOCK_NUM));

        let current_block = state.get_block(true).unwrap();

        let invalid_flashblock = Flashblock {
            index: 1,
            base: None,
            payload_id: PayloadId::new([0; 8]),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                transactions: vec![TX1_DATA, TX2_DATA],
                withdrawals: vec![],
                gas_used: 21000,
                ..Default::default()
            },
            metadata: Metadata {
                block_number: 1,
                receipts: HashMap::default(), // invalid because it's missing the receipts for txns
                new_account_balances: HashMap::default(),
            },
        };

        state.on_flashblock_received(invalid_flashblock);

        // When the flashblock is invalid, the chain doesn't progress
        assert_eq!(state.get_block(true).unwrap().hash(), current_block.hash());
    }

    #[test]
    fn test_get_balance() {
        let alice = Address::random();
        let bob = Address::random();

        let state = new_state();

        assert!(state.get_balance(alice).is_none());
        assert!(state.get_balance(bob).is_none());

        let mut fb1 = create_first_flashblock(BLOCK_NUM);
        fb1.metadata
            .new_account_balances
            .insert(alice, U256::from(1000));

        state.on_flashblock_received(fb1);
        assert_eq!(
            state.get_balance(alice).expect("should be set"),
            U256::from(1000)
        );
        assert!(state.get_balance(bob).is_none());

        let mut fb2 = create_second_flashblock();
        fb2.metadata
            .new_account_balances
            .insert(alice, U256::from(2000));
        fb2.metadata
            .new_account_balances
            .insert(bob, U256::from(1000));

        state.on_flashblock_received(fb2);
        assert_eq!(
            state.get_balance(alice).expect("should be set"),
            U256::from(2000)
        );
        assert_eq!(
            state.get_balance(bob).expect("should be set"),
            U256::from(1000)
        );
    }

    #[test]
    fn test_transaction_count() {
        let state = new_state();

        assert_eq!(
            state.get_transaction_count(BLOCK_INFO_SENDER),
            U256::from(0)
        );
        assert_eq!(state.get_transaction_count(TX1_SENDER), U256::from(0));
        assert_eq!(state.get_transaction_count(TX2_SENDER), U256::from(0));

        state.on_flashblock_received(create_first_flashblock(BLOCK_NUM));

        assert_eq!(
            state.get_transaction_count(BLOCK_INFO_SENDER),
            U256::from(1)
        );
        assert_eq!(state.get_transaction_count(TX1_SENDER), U256::from(0));
        assert_eq!(state.get_transaction_count(TX2_SENDER), U256::from(0));

        state.on_flashblock_received(create_second_flashblock());

        assert_eq!(
            state.get_transaction_count(BLOCK_INFO_SENDER),
            U256::from(1)
        );
        assert_eq!(state.get_transaction_count(TX1_SENDER), U256::from(1));
        assert_eq!(state.get_transaction_count(TX2_SENDER), U256::from(1));
    }

    #[test]
    fn test_get_receipt() {
        let state = new_state();
        assert!(state.get_transaction_receipt(BLOCK_INFO_HASH).is_none());
        assert!(state.get_transaction_receipt(TX1_HASH).is_none());
        assert!(state.get_transaction_receipt(TX2_HASH).is_none());

        state.on_flashblock_received(create_first_flashblock(BLOCK_NUM));
        let block_info_receipt = state.get_transaction_receipt(BLOCK_INFO_HASH).unwrap();

        assert_eq!(block_info_receipt.gas_used(), 10000);
        assert_eq!(block_info_receipt.cumulative_gas_used(), 10000);
        assert_eq!(block_info_receipt.block_number(), Some(BLOCK_NUM));

        assert!(state.get_transaction_receipt(TX1_HASH).is_none());
        assert!(state.get_transaction_receipt(TX2_HASH).is_none());

        state.on_flashblock_received(create_second_flashblock());

        let tx1_receipt = state.get_transaction_receipt(TX1_HASH).unwrap();
        assert_eq!(tx1_receipt.gas_used(), 11000);
        assert_eq!(tx1_receipt.cumulative_gas_used(), 21000);

        let tx2_receipt = state.get_transaction_receipt(TX2_HASH).unwrap();
        assert_eq!(tx2_receipt.gas_used(), 21000);
        assert_eq!(tx2_receipt.cumulative_gas_used(), 42000);
    }

    #[test]
    fn test_get_block() {
        let state = new_state();
        assert!(state.get_block(true).is_none());
        assert!(state.get_block(false).is_none());

        state.on_flashblock_received(create_first_flashblock(BLOCK_NUM));
        assert_eq!(
            state
                .get_block(false)
                .expect("should be set")
                .transactions
                .as_hashes()
                .expect("should be hashes"),
            vec![BLOCK_INFO_HASH,]
        );

        state.on_flashblock_received(create_second_flashblock());

        let block = state.get_block(false).unwrap();
        assert_eq!(
            block.transactions.as_hashes().expect("should be hashes"),
            vec![BLOCK_INFO_HASH, TX1_HASH, TX2_HASH,]
        );

        assert_eq!(block.header.state_root, B256::repeat_byte(0x1));
        assert_eq!(block.header.receipts_root, B256::repeat_byte(0x2));
        assert_eq!(block.header.gas_used, 21000);
        assert_eq!(block.header.base_fee_per_gas, Some(10000));
    }

    #[test]
    fn test_new_block_clears_current_block() {
        let state = new_state();
        state.on_flashblock_received(create_first_flashblock(BLOCK_NUM));
        state.on_flashblock_received(create_second_flashblock());

        let current_block = state.get_block(true).unwrap();

        assert_eq!(current_block.number(), BLOCK_NUM);
        assert_eq!(current_block.transactions.len(), 3);

        let new_block = create_first_flashblock(BLOCK_NUM + 1);
        state.on_flashblock_received(new_block);

        let updated_block = state.get_block(true).unwrap();
        assert_eq!(updated_block.number(), BLOCK_NUM + 1);
        assert_eq!(updated_block.transactions.len(), 1);
    }

    #[test]
    fn test_skip_initial_non_zero_index_payload() {
        let state = new_state();
        state.on_flashblock_received(create_first_flashblock(BLOCK_NUM));
        assert_eq!(
            state.get_block(true).expect("should be set").number(),
            BLOCK_NUM
        );

        let payload = Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 1, // Non-zero index but no base in cache
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: Metadata {
                block_number: BLOCK_NUM + 1,
                ..Default::default()
            },
        };

        state.on_flashblock_received(payload);

        assert!(state.get_block(true).is_none());
    }

    #[test]
    fn test_non_sequential_payload_ignored() {
        let state = new_state();
        assert!(state.get_block(true).is_none());
        state.on_flashblock_received(create_first_flashblock(BLOCK_NUM));
        // Just the block info transaction
        assert_eq!(
            state
                .get_block(true)
                .expect("should be set")
                .transactions
                .len(),
            1
        );

        let mut third_payload = create_second_flashblock();
        third_payload.index = 3;

        state.on_flashblock_received(third_payload);
        // Still the block info transaction, the txns in the third payload are ignored as it's
        // missing a Flashblock
        assert_eq!(
            state
                .get_block(true)
                .expect("should be set")
                .transactions
                .len(),
            1
        );
    }
}
