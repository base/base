//! Shared test environment for Base Azul action tests.

use alloy_primitives::{Address, Bytes, TxKind, U256};
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, L2Sequencer,
    SharedL1Chain, TEST_ACCOUNT_ADDRESS, TestRollupConfigBuilder, TestRollupNode, VerifierPipeline,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_consensus::{BaseBlock, BaseTxEnvelope};

/// Test environment preconfigured to cross the Base Azul activation at L2 block 3.
pub(crate) struct AzulTestEnv {
    /// Sequencer used to build probe deployment and call blocks.
    pub(crate) sequencer: L2Sequencer,
    harness: ActionTestHarness,
    batcher_cfg: BatcherConfig,
    node: TestRollupNode<VerifierPipeline>,
    chain: SharedL1Chain,
    chain_id: u64,
}

impl AzulTestEnv {
    /// Creates an environment with all forks through Jovian active at genesis
    /// and Base Azul active at timestamp 6.
    pub(crate) fn new() -> Self {
        let batcher_cfg = BatcherConfig {
            encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
            ..Default::default()
        };

        let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
            .through_isthmus()
            .with_jovian_at(0)
            .with_azul_at(6)
            .build();
        let chain_id = rollup_cfg.l2_chain_id.id();
        let harness = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

        let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());
        let mut sequencer = harness.create_l2_sequencer(l1_chain);

        let (node, chain) = harness.create_test_rollup_node_from_sequencer(
            &mut sequencer,
            SharedL1Chain::from_blocks(harness.l1.chain().to_vec()),
        );

        Self { sequencer, harness, batcher_cfg, node, chain, chain_id }
    }

    /// Returns the address created by the first test-account deployment.
    pub(crate) fn first_contract_address(&self) -> Address {
        TEST_ACCOUNT_ADDRESS.create(0)
    }

    /// Creates and signs a test-account transaction.
    pub(crate) fn create_tx(
        &self,
        to: TxKind,
        input: Bytes,
        value: U256,
        gas_limit: u64,
    ) -> BaseTxEnvelope {
        let account = self.sequencer.test_account();
        let mut account = account.lock().expect("test account lock");
        account.create_tx(self.chain_id, to, input, value, gas_limit)
    }

    /// Batches the supplied L2 blocks, derives each one, and asserts the final safe head.
    pub(crate) async fn derive_blocks<const N: usize>(
        &mut self,
        blocks: [(BaseBlock, u64); N],
        expected_safe_head: u64,
        boundary: &str,
    ) {
        let mut batcher = Batcher::new(
            ActionL2Source::new(),
            &self.harness.rollup_config,
            self.batcher_cfg.clone(),
        );
        self.node.initialize().await;

        for (block, i) in blocks {
            batcher.push_block(block);
            batcher.advance(&mut self.harness.l1).await;
            self.chain.push(self.harness.l1.tip().clone());
            let derived = self.node.run_until_idle().await;
            assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");
        }

        assert_eq!(
            self.node.l2_safe_number(),
            expected_safe_head,
            "all {expected_safe_head} L2 blocks must derive through the {boundary} boundary"
        );
    }
}

impl Default for AzulTestEnv {
    fn default() -> Self {
        Self::new()
    }
}
