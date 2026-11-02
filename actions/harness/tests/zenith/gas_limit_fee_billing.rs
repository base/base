//! Gas-limit fee billing across the Zenith boundary: the production payload builder bills a
//! transaction for its declared `gas_limit` from Zenith, the block accounts for that gas, and the
//! verifier re-derives the same blocks.

use alloy_consensus::{BlockHeader, TxReceipt};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, L2Sequencer,
    SharedL1Chain, TestRollupConfigBuilder, TestRollupNode, VerifierPipeline,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};

/// L2 timestamp where Zenith activates: block 1 (timestamp 2) is pre-Zenith, block 2 (timestamp 4)
/// is the first Zenith block.
const ZENITH_ACTIVATION_TIMESTAMP: u64 = 4;
/// Gas limit of the transfers, far above the 21 000 a plain transfer consumes.
const TRANSFER_GAS_LIMIT: u64 = 100_000;
/// Gas a plain value transfer consumes.
const TRANSFER_GAS_USED: u64 = 21_000;

/// Environment with every fork through Everest active at genesis and Zenith at timestamp 4.
struct ZenithTestEnv {
    sequencer: L2Sequencer,
    harness: ActionTestHarness,
    batcher_cfg: BatcherConfig,
    node: TestRollupNode<VerifierPipeline>,
    chain: SharedL1Chain,
}

impl ZenithTestEnv {
    fn new() -> Self {
        let batcher_cfg = BatcherConfig {
            encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
            ..Default::default()
        };
        let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
            .through_isthmus()
            .with_jovian_at(0)
            .with_azul_at(0)
            .with_beryl_at(0)
            .with_cobalt_at(0)
            .with_everest_at(0)
            .with_zenith_at(ZENITH_ACTIVATION_TIMESTAMP)
            .build();
        let harness = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

        let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
        let mut sequencer = harness.create_l2_sequencer(l1_chain);
        let (node, chain) = harness.create_test_rollup_node_from_sequencer(
            &mut sequencer,
            SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
        );

        Self { sequencer, harness, batcher_cfg, node, chain }
    }

    /// Builds the next block with one over-provisioned transfer from the harness test account.
    async fn build_block_with_transfer(&mut self) -> BaseBlock {
        let tx = {
            let account = self.sequencer.test_account();
            let mut account = account.lock().expect("test account lock");
            account.create_tx(
                self.harness.rollup_config.l2_chain_id.id(),
                TxKind::Call(Address::ZERO),
                Bytes::new(),
                U256::from(1),
                TRANSFER_GAS_LIMIT,
            )
        };
        self.sequencer.build_next_block_with_transactions(vec![tx]).await
    }

    /// Receipts of `block`, split into the leading deposits and the user transactions.
    fn receipts(&self, block: &BaseBlock) -> (Vec<BaseReceipt>, Vec<BaseReceipt>) {
        let deposit_count = block
            .body
            .transactions
            .iter()
            .take_while(|tx| matches!(tx, BaseTxEnvelope::Deposit(_)))
            .count();
        let mut receipts = self
            .sequencer
            .receipts_at(block.header.number)
            .unwrap_or_else(|| panic!("receipts must exist for L2 block {}", block.header.number));
        let user = receipts.split_off(deposit_count);
        (receipts, user)
    }

    /// Batches `blocks`, derives them on the verifier, and asserts the safe head reaches the last.
    async fn derive_blocks(&mut self, blocks: Vec<BaseBlock>) {
        let batcher = Batcher::new(
            ActionL2Source::new(),
            &self.harness.rollup_config,
            self.batcher_cfg.clone(),
        );
        self.node.initialize().await;

        let expected_safe_head = blocks.last().map_or(0, |block| block.header.number);
        for block in blocks {
            let number = block.header.number;
            batcher.push_block(block);
            batcher.advance(&mut self.harness.l1).await;
            self.chain.push(self.harness.l1.tip().clone());
            let derived = self.node.run_until_idle().await;
            assert_eq!(derived, 1, "L2 block {number} must derive exactly once");
        }
        assert_eq!(
            self.node.l2_safe_number(),
            expected_safe_head,
            "the verifier must derive every block across the Zenith boundary"
        );
    }
}

/// The gas a block's receipts attribute to each transaction, as `(deposit gas, user gas)`.
fn gas_per_transaction(deposits: &[BaseReceipt], user: &[BaseReceipt]) -> (Vec<u64>, Vec<u64>) {
    let mut previous = 0;
    let mut per_tx = |receipt: &BaseReceipt| {
        let cumulative = receipt.cumulative_gas_used();
        let gas = cumulative - previous;
        previous = cumulative;
        gas
    };
    let deposit_gas = deposits.iter().map(&mut per_tx).collect();
    let user_gas = user.iter().map(&mut per_tx).collect();
    (deposit_gas, user_gas)
}

/// Before Zenith a transfer with a 100k gas limit is accounted at the 21k it consumes; from Zenith
/// it is accounted at the full 100k, the block's `gas_used` follows, deposits are untouched, and
/// the verifier re-derives both blocks.
#[tokio::test]
async fn zenith_bills_transfers_for_their_gas_limit_and_derives() {
    let mut env = ZenithTestEnv::new();

    let pre_zenith = env.build_block_with_transfer().await;
    assert!(pre_zenith.header.timestamp() < ZENITH_ACTIVATION_TIMESTAMP);
    let zenith = env.build_block_with_transfer().await;
    assert_eq!(zenith.header.timestamp(), ZENITH_ACTIVATION_TIMESTAMP);

    let mut deposit_gas_per_block = Vec::new();
    for (block, expected_user_gas) in
        [(&pre_zenith, TRANSFER_GAS_USED), (&zenith, TRANSFER_GAS_LIMIT)]
    {
        let number = block.header.number;
        let (deposits, user) = env.receipts(block);
        assert!(user.iter().all(TxReceipt::status), "block {number}: transfer must succeed");
        let (deposit_gas, user_gas) = gas_per_transaction(&deposits, &user);
        deposit_gas_per_block.push(deposit_gas.clone());
        assert_eq!(
            user_gas,
            vec![expected_user_gas],
            "block {number}: the transfer must be accounted at {expected_user_gas} gas"
        );
        assert!(
            deposit_gas.iter().all(|gas| *gas < TRANSFER_GAS_LIMIT),
            "block {number}: deposits must keep their actual gas, got {deposit_gas:?}"
        );
        assert_eq!(
            block.header.gas_used(),
            user.last().expect("one user receipt").cumulative_gas_used(),
            "block {number}: header gas_used must equal the receipts' cumulative gas"
        );
    }

    assert_eq!(
        deposit_gas_per_block[0], deposit_gas_per_block[1],
        "the L1 info deposit must cost the same gas before and after Zenith"
    );

    env.derive_blocks(vec![pre_zenith, zenith]).await;
}
