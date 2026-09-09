//! Shared test environment for Base Zenith action tests.

use alloy_primitives::{Bytes, U256};
use alloy_signer::SignerSync;
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, L2Sequencer,
    SharedL1Chain, TestRollupConfigBuilder, TestRollupNode, VerifierPipeline,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_consensus::{
    BaseBlock, BaseReceipt, BaseTxEnvelope, Call, Eip8130Signed, TxEip8130,
};
use base_test_utils::Account;

/// L2 timestamp where the Zenith fork activates in these tests.
pub(crate) const ZENITH_ACTIVATION_TIMESTAMP: u64 = 4;

/// Test environment preconfigured to cross the Base Zenith activation at L2 block 2.
pub(crate) struct ZenithTestEnv {
    /// Sequencer used to build Zenith test blocks.
    pub(crate) sequencer: L2Sequencer,
    harness: ActionTestHarness,
    batcher_cfg: BatcherConfig,
    node: TestRollupNode<VerifierPipeline>,
    chain: SharedL1Chain,
    chain_id: u64,
}

impl ZenithTestEnv {
    /// Creates an environment with all forks through Jovian active at genesis
    /// and Base Zenith active at timestamp 4.
    pub(crate) fn new() -> Self {
        let batcher_cfg = BatcherConfig {
            encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
            ..Default::default()
        };

        let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
            .through_isthmus()
            .with_jovian_at(0)
            .with_azul_at(0)
            .with_beryl_at(0)
            .with_cobalt_at(ZENITH_ACTIVATION_TIMESTAMP)
            .with_zenith_at(ZENITH_ACTIVATION_TIMESTAMP)
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

    /// Creates a signed EOA-path EIP-8130 transaction bound to this
    /// environment's chain, via the shared [`Self::eip8130_user_tx`] builder.
    pub(crate) fn create_eip8130_tx(&self, nonce_sequence: u64) -> BaseTxEnvelope {
        Self::eip8130_user_tx(self.chain_id, nonce_sequence)
    }

    /// Batches the supplied L2 blocks, derives each one, and asserts the final safe head.
    pub(crate) async fn derive_blocks<const N: usize>(
        &mut self,
        blocks: [(BaseBlock, u64); N],
        expected_safe_head: u64,
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
            "all {expected_safe_head} L2 blocks must derive through the Zenith boundary"
        );
    }

    /// Returns the receipt for a non-deposit transaction in `block`.
    pub(crate) fn user_tx_receipt(&self, block: &BaseBlock, user_tx_index: usize) -> BaseReceipt {
        let deposit_count = block
            .body
            .transactions
            .iter()
            .take_while(|tx| matches!(tx, BaseTxEnvelope::Deposit(_)))
            .count();
        let receipts = self
            .sequencer
            .receipts_at(block.header.number)
            .unwrap_or_else(|| panic!("receipts must exist for L2 block {}", block.header.number));
        receipts
            .into_iter()
            .nth(deposit_count + user_tx_index)
            .unwrap_or_else(|| panic!("user tx receipt {user_tx_index} must exist"))
    }

    /// Builds a signed EOA-path EIP-8130 transaction from Alice with a single
    /// value-less call to Bob. The sender is recovered from the signature
    /// (`sender: None`), and Alice self-pays (`payer: None`).
    ///
    /// Exposed as an associated function (no `self`) so derivation tests that
    /// build their own harness can reuse it without constructing a full
    /// [`ZenithTestEnv`].
    pub(crate) fn eip8130_user_tx(chain_id: u64, nonce_sequence: u64) -> BaseTxEnvelope {
        let alice = Account::Alice;

        let tx = TxEip8130 {
            chain_id,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1_000_000_000,
            gas_limit: 200_000,
            account_changes: Vec::new(),
            calls: vec![vec![Call { to: Account::Bob.address(), data: Bytes::new() }]],
            metadata: Bytes::new(),
            payer: None,
        };

        let signature = alice
            .signer()
            .sign_hash_sync(&tx.sender_signature_hash())
            .expect("test transaction signing must succeed");

        let signed = Eip8130Signed::new(tx, signature.as_bytes().to_vec().into(), Bytes::new());

        signed.into()
    }
}

impl Default for ZenithTestEnv {
    fn default() -> Self {
        Self::new()
    }
}
