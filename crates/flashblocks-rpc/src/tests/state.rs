#[cfg(test)]
mod tests {
    use crate::rpc::FlashblocksAPI;
    use crate::state::FlashblocksState;
    use crate::subscription::{Flashblock, FlashblocksReceiver, Metadata};
    use crate::tests::{BLOCK_INFO_TXN, BLOCK_INFO_TXN_HASH};
    use alloy_consensus::crypto::secp256k1::public_key_to_address;
    use alloy_consensus::{BlockHeader, Receipt};
    use alloy_consensus::{Header, Transaction};
    use alloy_eips::{BlockHashOrNumber, Decodable2718, Encodable2718};
    use alloy_genesis::GenesisAccount;
    use alloy_primitives::map::foldhash::HashMap;
    use alloy_primitives::{Address, BlockNumber, Bytes, B256, U256};
    use alloy_provider::network::BlockResponse;
    use alloy_rpc_types_engine::PayloadId;
    use op_alloy_consensus::OpDepositReceipt;
    use reth::builder::NodeTypesWithDBAdapter;
    use reth::providers::{AccountReader, BlockNumReader, BlockReader};
    use reth::revm::database::StateProviderDatabase;
    use reth::transaction_pool::test_utils::TransactionBuilder;
    use reth_db::{test_utils::TempDatabase, DatabaseEnv};
    use reth_evm::execute::Executor;
    use reth_evm::ConfigureEvm;
    use reth_optimism_chainspec::{OpChainSpecBuilder, BASE_MAINNET};
    use reth_optimism_evm::OpEvmConfig;
    use reth_optimism_node::OpNode;
    use reth_optimism_primitives::{OpBlock, OpBlockBody, OpReceipt, OpTransactionSigned};
    use reth_primitives_traits::{Account, Block, RecoveredBlock, SealedHeader};
    use reth_provider::providers::BlockchainProvider;
    use reth_provider::test_utils::create_test_provider_factory_with_node_types;
    use reth_provider::{
        BlockWriter, ChainSpecProvider, ExecutionOutcome, LatestStateProviderRef, ProviderFactory,
    };
    use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
    use std::sync::Arc;

    #[derive(Eq, PartialEq, Debug, Hash, Clone, Copy)]
    enum User {
        Alice,
        Bob,
        Charlie,
    }

    type NodeTypes = NodeTypesWithDBAdapter<OpNode, Arc<TempDatabase<DatabaseEnv>>>;

    #[derive(Debug, Clone)]
    struct TestHarness {
        flashblocks: FlashblocksState<BlockchainProvider<NodeTypes>>,
        provider: BlockchainProvider<NodeTypes>,
        factory: ProviderFactory<NodeTypes>,
        user_to_address: HashMap<User, Address>,
        user_to_private_key: HashMap<User, B256>,
    }

    impl TestHarness {
        fn address(&self, u: User) -> Address {
            assert!(self.user_to_address.contains_key(&u));
            self.user_to_address[&u]
        }

        fn signer(&self, u: User) -> B256 {
            assert!(self.user_to_private_key.contains_key(&u));
            self.user_to_private_key[&u]
        }

        fn current_canonical_block(&self) -> RecoveredBlock<OpBlock> {
            let latest_block_num = self
                .provider
                .last_block_number()
                .expect("should be a latest block");

            self.provider
                .block(BlockHashOrNumber::Number(latest_block_num))
                .expect("able to load block")
                .expect("block exists")
                .try_into_recovered()
                .expect("able to recover block")
        }

        fn account_state(&self, u: User) -> Account {
            let basic_account = self
                .provider
                .basic_account(&self.address(u))
                .expect("can lookup account state")
                .expect("should be existing account state");

            let nonce = self
                .flashblocks
                .get_transaction_count(self.address(u))
                .to::<u64>();
            let balance = self
                .flashblocks
                .get_balance(self.address(u))
                .unwrap_or(basic_account.balance);

            Account {
                nonce,
                balance,
                bytecode_hash: basic_account.bytecode_hash,
            }
        }

        fn build_transaction_to_send_eth(
            &self,
            from: User,
            to: User,
            amount: u128,
        ) -> OpTransactionSigned {
            let txn = TransactionBuilder::default()
                .signer(self.signer(from))
                .to(self.address(to))
                .nonce(self.account_state(from).nonce)
                .value(amount)
                .gas_limit(21_000)
                .max_fee_per_gas(200)
                .into_eip1559()
                .as_eip1559()
                .unwrap()
                .clone();

            OpTransactionSigned::Eip1559(txn)
        }

        fn new_canonical_block(&mut self, mut user_transactions: Vec<OpTransactionSigned>) {
            let current_tip = self.current_canonical_block();

            let deposit_transaction =
                OpTransactionSigned::decode_2718_exact(BLOCK_INFO_TXN.iter().as_slice()).unwrap();

            let mut transactions: Vec<OpTransactionSigned> = vec![deposit_transaction];
            transactions.append(&mut user_transactions);

            let block = OpBlock::new_sealed(
                SealedHeader::new_unhashed(Header {
                    parent_beacon_block_root: Some(current_tip.hash()),
                    parent_hash: current_tip.hash(),
                    number: current_tip.number() + 1,
                    timestamp: current_tip.header().timestamp() + 2,
                    gas_limit: current_tip.header().gas_limit(),
                    ..Header::default()
                }),
                OpBlockBody {
                    transactions,
                    ommers: vec![],
                    withdrawals: None,
                },
            )
            .try_recover()
            .expect("able to recover block");

            let provider = self.factory.provider().unwrap();

            // Execute the block to produce a block execution output
            let mut block_execution_output = OpEvmConfig::optimism(self.provider.chain_spec())
                .batch_executor(StateProviderDatabase::new(LatestStateProviderRef::new(
                    &provider,
                )))
                .execute(&block)
                .unwrap();

            block_execution_output.state.reverts.sort();

            let execution_outcome = ExecutionOutcome {
                bundle: block_execution_output.state.clone(),
                receipts: vec![block_execution_output.receipts.clone()],
                first_block: block.number,
                requests: vec![block_execution_output.requests.clone()],
            };

            // Commit the block's execution outcome to the database
            let provider_rw = self.factory.provider_rw().unwrap();
            provider_rw
                .append_blocks_with_state(
                    vec![block.clone()],
                    &execution_outcome,
                    Default::default(),
                    Default::default(),
                )
                .unwrap();
            provider_rw.commit().unwrap();

            self.flashblocks.on_canonical_block_received(&block);
        }

        fn new() -> Self {
            let keys = reth_testing_utils::generators::generate_keys(&mut rand::rng(), 3);
            let alice_signer = keys[0];
            let bob_signer = keys[1];
            let charli_signer = keys[2];

            let alice = public_key_to_address(alice_signer.public_key());
            let bob = public_key_to_address(bob_signer.public_key());
            let charlie = public_key_to_address(charli_signer.public_key());

            let items = vec![
                (
                    alice,
                    GenesisAccount::default().with_balance(U256::from(100_000_000)),
                ),
                (
                    bob,
                    GenesisAccount::default().with_balance(U256::from(100_000_000)),
                ),
                (
                    charlie,
                    GenesisAccount::default().with_balance(U256::from(100_000_000)),
                ),
            ];

            let genesis = BASE_MAINNET
                .genesis
                .clone()
                .extend_accounts(items)
                .with_gas_limit(100_000_000);

            let chain_spec = OpChainSpecBuilder::base_mainnet()
                .genesis(genesis)
                .isthmus_activated()
                .build();

            let factory =
                create_test_provider_factory_with_node_types::<OpNode>(Arc::new(chain_spec));

            assert!(reth_db_common::init::init_genesis(&factory).is_ok());

            let provider =
                BlockchainProvider::new(factory.clone()).expect("able to setup provider");

            let block = provider
                .block(BlockHashOrNumber::Number(0))
                .expect("able to load block")
                .expect("block exists")
                .try_into_recovered()
                .expect("able to recover block");

            let flashblocks = FlashblocksState::new(provider.clone());

            flashblocks.on_canonical_block_received(&block);

            Self {
                factory,
                flashblocks,
                provider,
                user_to_address: {
                    let mut res = HashMap::default();
                    res.insert(User::Alice, alice);
                    res.insert(User::Bob, bob);
                    res.insert(User::Charlie, charlie);
                    res
                },
                user_to_private_key: {
                    let mut res = HashMap::default();
                    res.insert(User::Alice, alice_signer.secret_bytes().into());
                    res.insert(User::Bob, bob_signer.secret_bytes().into());
                    res.insert(User::Charlie, charli_signer.secret_bytes().into());
                    res
                },
            }
        }
    }

    struct FlashblockBuilder {
        transactions: Vec<Bytes>,
        receipts: HashMap<B256, OpReceipt>,
        harness: TestHarness,
        canonical_block_number: Option<BlockNumber>,
        index: u64,
    }

    impl FlashblockBuilder {
        pub fn new_base(harness: &TestHarness) -> Self {
            Self {
                canonical_block_number: None,
                transactions: vec![BLOCK_INFO_TXN],
                receipts: {
                    let mut receipts = alloy_primitives::map::HashMap::default();
                    receipts.insert(
                        BLOCK_INFO_TXN_HASH,
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
                index: 0,
                harness: harness.clone(),
            }
        }
        pub fn new(harness: &TestHarness, index: u64) -> Self {
            Self {
                canonical_block_number: None,
                transactions: Vec::new(),
                receipts: HashMap::default(),
                harness: harness.clone(),
                index,
            }
        }

        pub fn with_receipts(&mut self, receipts: HashMap<B256, OpReceipt>) -> &mut Self {
            self.receipts = receipts;
            self
        }

        pub fn with_transactions(&mut self, transactions: Vec<OpTransactionSigned>) -> &mut Self {
            assert_ne!(self.index, 0, "Cannot set txns for initial flashblock");
            self.transactions.clear();

            let mut cumulative_gas_used = 0;
            for txn in transactions.iter() {
                cumulative_gas_used = cumulative_gas_used + txn.gas_limit();
                self.transactions.push(txn.encoded_2718().into());
                self.receipts.insert(
                    txn.hash().clone(),
                    OpReceipt::Eip1559(Receipt {
                        status: true.into(),
                        cumulative_gas_used,
                        logs: vec![],
                    }),
                );
            }
            self
        }

        pub fn with_canonical_block_number(&mut self, num: BlockNumber) -> &mut Self {
            self.canonical_block_number = Some(num);
            self
        }

        pub fn build(&self) -> Flashblock {
            let current_block = self.harness.current_canonical_block();
            let canonical_block_num = self
                .canonical_block_number
                .unwrap_or_else(|| current_block.number)
                + 1;

            let base = if self.index == 0 {
                Some(ExecutionPayloadBaseV1 {
                    parent_beacon_block_root: current_block.hash(),
                    parent_hash: current_block.hash(),
                    fee_recipient: Address::random(),
                    prev_randao: B256::random(),
                    block_number: canonical_block_num,
                    gas_limit: current_block.gas_limit,
                    timestamp: current_block.timestamp + 2,
                    extra_data: Bytes::new(),
                    base_fee_per_gas: U256::from(100),
                })
            } else {
                None
            };

            Flashblock {
                payload_id: PayloadId::default(),
                index: self.index,
                base,
                diff: ExecutionPayloadFlashblockDeltaV1 {
                    state_root: B256::default(),
                    receipts_root: B256::default(),
                    block_hash: B256::default(),
                    gas_used: 0,
                    withdrawals: Vec::new(),
                    logs_bloom: Default::default(),
                    withdrawals_root: Default::default(),
                    transactions: self.transactions.clone(),
                },
                metadata: Metadata {
                    block_number: canonical_block_num,
                    receipts: self.receipts.clone(),
                    new_account_balances: HashMap::default(),
                },
            }
        }
    }

    #[test]
    fn test_state_overrides_persisted_across_flashblocks() {
        reth_tracing::init_test_tracing();
        let test = TestHarness::new();

        test.flashblocks
            .on_flashblock_received(FlashblockBuilder::new_base(&test).build());

        assert_eq!(
            test.flashblocks
                .get_block(true)
                .expect("block is built")
                .transactions
                .len(),
            1
        );

        assert!(test.flashblocks.get_state_overrides().is_some());
        assert!(!test
            .flashblocks
            .get_state_overrides()
            .unwrap()
            .contains_key(&test.address(User::Alice)));

        test.flashblocks.on_flashblock_received(
            FlashblockBuilder::new(&test, 1)
                .with_transactions(vec![test.build_transaction_to_send_eth(
                    User::Alice,
                    User::Bob,
                    100_000,
                )])
                .build(),
        );

        let pending = test.flashblocks.get_block(true);
        assert!(pending.is_some());
        let pending = pending.unwrap();
        assert_eq!(pending.transactions.len(), 2);

        let overrides = test
            .flashblocks
            .get_state_overrides()
            .expect("should be set from txn execution");

        assert!(overrides.get(&test.address(User::Alice)).is_some());
        assert_eq!(
            overrides
                .get(&test.address(User::Bob))
                .expect("should be set as txn receiver")
                .balance
                .expect("should be changed due to receiving funds"),
            U256::from(100_100_000)
        );

        test.flashblocks
            .on_flashblock_received(FlashblockBuilder::new(&test, 2).build());

        let overrides = test
            .flashblocks
            .get_state_overrides()
            .expect("should be set from txn execution in flashblock index 1");

        assert!(overrides.get(&test.address(User::Alice)).is_some());
        assert_eq!(
            overrides
                .get(&test.address(User::Bob))
                .expect("should be set as txn receiver")
                .balance
                .expect("should be changed due to receiving funds"),
            U256::from(100_100_000)
        );
    }

    #[test]
    fn test_state_overrides_persisted_across_blocks() {
        reth_tracing::init_test_tracing();
        let test = TestHarness::new();

        let initial_base = FlashblockBuilder::new_base(&test).build();
        let initial_block_number = initial_base.metadata.block_number;
        test.flashblocks.on_flashblock_received(initial_base);
        assert_eq!(
            test.flashblocks
                .get_block(true)
                .expect("block is built")
                .transactions
                .len(),
            1
        );

        assert!(test.flashblocks.get_state_overrides().is_some());
        assert!(!test
            .flashblocks
            .get_state_overrides()
            .unwrap()
            .contains_key(&test.address(User::Alice)));

        test.flashblocks.on_flashblock_received(
            FlashblockBuilder::new(&test, 1)
                .with_transactions(vec![test.build_transaction_to_send_eth(
                    User::Alice,
                    User::Bob,
                    100_000,
                )])
                .build(),
        );

        let pending = test.flashblocks.get_block(true);
        assert!(pending.is_some());
        let pending = pending.unwrap();
        assert_eq!(pending.transactions.len(), 2);

        let overrides = test
            .flashblocks
            .get_state_overrides()
            .expect("should be set from txn execution");

        assert!(overrides.get(&test.address(User::Alice)).is_some());
        assert_eq!(
            overrides
                .get(&test.address(User::Bob))
                .expect("should be set as txn receiver")
                .balance
                .expect("should be changed due to receiving funds"),
            U256::from(100_100_000)
        );

        test.flashblocks.on_flashblock_received(
            FlashblockBuilder::new_base(&test)
                .with_canonical_block_number(initial_block_number)
                .build(),
        );

        assert_eq!(
            test.flashblocks
                .get_block(true)
                .expect("block is built")
                .transactions
                .len(),
            1
        );
        assert_eq!(
            test.flashblocks
                .get_block(true)
                .expect("block is built")
                .header
                .number,
            initial_block_number + 1
        );

        assert!(test.flashblocks.get_state_overrides().is_some());
        assert!(test
            .flashblocks
            .get_state_overrides()
            .unwrap()
            .contains_key(&test.address(User::Alice)));

        test.flashblocks.on_flashblock_received(
            FlashblockBuilder::new(&test, 1)
                .with_canonical_block_number(initial_block_number)
                .with_transactions(vec![test.build_transaction_to_send_eth(
                    User::Alice,
                    User::Bob,
                    100_000,
                )])
                .build(),
        );

        let overrides = test
            .flashblocks
            .get_state_overrides()
            .expect("should be set from txn execution");

        assert!(overrides.get(&test.address(User::Alice)).is_some());
        assert_eq!(
            overrides
                .get(&test.address(User::Bob))
                .expect("should be set as txn receiver")
                .balance
                .expect("should be changed due to receiving funds"),
            U256::from(100_200_000)
        );
    }

    #[test]
    fn test_missing_receipts_will_not_process() {
        reth_tracing::init_test_tracing();
        let test = TestHarness::new();

        test.flashblocks
            .on_flashblock_received(FlashblockBuilder::new_base(&test).build());

        let current_block = test.flashblocks.get_block(true);

        test.flashblocks.on_flashblock_received(
            FlashblockBuilder::new(&test, 1)
                .with_transactions(vec![test.build_transaction_to_send_eth(
                    User::Alice,
                    User::Bob,
                    100,
                )])
                .with_receipts(HashMap::default()) // Clear the receipts
                .build(),
        );

        let pending_block = test.flashblocks.get_block(true);

        // When the flashblock is invalid, the chain doesn't progress
        assert_eq!(pending_block.unwrap().hash(), current_block.unwrap().hash());
    }

    #[test]
    fn test_flashblock_for_new_canonical_block_clears_older_flashblocks() {
        reth_tracing::init_test_tracing();
        let test = TestHarness::new();

        test.flashblocks
            .on_flashblock_received(FlashblockBuilder::new_base(&test).build());

        let current_block = test.flashblocks.get_block(true).expect("should be a block");

        assert_eq!(current_block.header().number, 1);
        assert_eq!(current_block.transactions.len(), 1);

        test.flashblocks.on_flashblock_received(
            FlashblockBuilder::new(&test, 1)
                .with_canonical_block_number(100)
                .build(),
        );

        let current_block = test.flashblocks.get_block(true);
        assert!(current_block.is_none());
    }

    #[test]
    fn test_non_sequential_payload_ignored() {
        reth_tracing::init_test_tracing();
        let test = TestHarness::new();

        assert!(test.flashblocks.get_block(true).is_none());

        test.flashblocks
            .on_flashblock_received(FlashblockBuilder::new_base(&test).build());

        // Just the block info transaction
        assert_eq!(
            test.flashblocks
                .get_block(true)
                .expect("should be set")
                .transactions
                .len(),
            1
        );

        test.flashblocks.on_flashblock_received(
            FlashblockBuilder::new(&test, 3)
                .with_transactions(vec![test.build_transaction_to_send_eth(
                    User::Alice,
                    User::Bob,
                    100,
                )])
                .build(),
        );

        // Still the block info transaction, the txns in the third payload are ignored as it's
        // missing a Flashblock
        assert_eq!(
            test.flashblocks
                .get_block(true)
                .expect("should be set")
                .transactions
                .len(),
            1
        );
    }

    #[test]
    fn test_canonical_block_clears_older_flashblocks() {
        reth_tracing::init_test_tracing();
        let test = TestHarness::new();

        test.flashblocks
            .on_flashblock_received(FlashblockBuilder::new_base(&test).build());

        assert_eq!(
            test.flashblocks
                .get_block(true)
                .expect("should have pending block one")
                .number(),
            1
        );

        let block = OpBlock::new_sealed(
            SealedHeader::new_unhashed(Header {
                number: 3, // Greater than the current Flashblock block number of 1
                ..Header::default()
            }),
            OpBlockBody::default(),
        )
        .try_recover()
        .expect("able to recover block");

        test.flashblocks.on_canonical_block_received(&block);

        // Flashblock is cleared, as canonical data is more recent
        assert!(test.flashblocks.get_block(true).is_none());
    }

    #[test]
    fn test_duplicate_flashblock_ignored() {
        reth_tracing::init_test_tracing();
        let test = TestHarness::new();

        test.flashblocks
            .on_flashblock_received(FlashblockBuilder::new_base(&test).build());

        let fb = FlashblockBuilder::new(&test, 1)
            .with_transactions(vec![test.build_transaction_to_send_eth(
                User::Alice,
                User::Bob,
                100_000,
            )])
            .build();

        test.flashblocks.on_flashblock_received(fb.clone());
        let block = test.flashblocks.get_block(true);

        test.flashblocks.on_flashblock_received(fb.clone());
        let block_two = test.flashblocks.get_block(true);

        assert_eq!(block, block_two);
    }

    #[test]
    fn test_progress_canonical_blocks_without_flashblocks() {
        reth_tracing::init_test_tracing();
        let mut test = TestHarness::new();

        let genesis_block = test.current_canonical_block();
        assert_eq!(genesis_block.number, 0);
        assert_eq!(genesis_block.transaction_count(), 0);
        assert!(test.flashblocks.get_block(true).is_none());

        test.new_canonical_block(vec![test.build_transaction_to_send_eth(
            User::Alice,
            User::Bob,
            100,
        )]);

        let block_one = test.current_canonical_block();
        assert_eq!(block_one.number, 1);
        assert_eq!(block_one.transaction_count(), 2);
        assert!(test.flashblocks.get_block(true).is_none());

        test.new_canonical_block(vec![
            test.build_transaction_to_send_eth(User::Bob, User::Charlie, 100),
            test.build_transaction_to_send_eth(User::Charlie, User::Alice, 1000),
        ]);

        let block_one = test.current_canonical_block();
        assert_eq!(block_one.number, 2);
        assert_eq!(block_one.transaction_count(), 3);
        assert!(test.flashblocks.get_block(true).is_none());
    }
}
