//! Derivation-layer rejection of pre-Zenith EIP-8130 transactions.

use alloy_primitives::B256;
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

use crate::env::ZenithTestEnv;

/// An EIP-8130 transaction batched into a pre-Zenith L2 block is dropped by the
/// verifier's derivation pipeline (`BatchValidity::Drop::Eip8130PreZenith`), and
/// the pipeline force-includes a deposit-only block in its place once the
/// sequencing window closes.
///
/// Setup (`block_time` = 2, Zenith at ts = 8 → L2 block 4):
/// ```text
///   seq_window_size = 4  (epoch-0 window closes at L1 block 4)
///   L1 block 1: batch for L2 block 1 (user tx)          → derived normally
///   L1 block 2: batch for L2 block 2 (user tx)          → derived normally
///   L1 block 3: batch for L2 block 3 (EIP-8130, ts = 6) → DROPPED (pre-Zenith)
///   L1 block 4: empty (closes epoch-0 window)           → block 3 force-included
///                                                          as deposit-only
/// ```
#[tokio::test]
async fn eip8130_batch_is_dropped_before_zenith() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };

    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(0)
        .with_azul_at(0)
        .with_beryl_at(0)
        .with_cobalt_at(8)
        .with_zenith_at(8)
        .with_seq_window_size(4)
        .build();
    let chain_id = rollup_cfg.l2_chain_id.id();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Build three pre-Zenith L2 blocks. Blocks 1-2 carry a normal user tx; block
    // 3 (ts = 6) carries an EIP-8130 transaction whose batch the verifier must
    // drop because Zenith is not active until ts = 8.
    let mut eip8130_block_hash = B256::ZERO;
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg);
    for i in 1u64..=3 {
        if i == 3 {
            let tx = ZenithTestEnv::eip8130_user_tx(chain_id, 0);
            let block = builder.build_next_block_with_transactions(vec![tx]).await;
            eip8130_block_hash = block.header.hash_slow();
            batcher.push_block(block);
        } else {
            batcher.push_block(builder.build_next_block_with_single_transaction().await);
        }
        batcher.advance(&mut h.l1).await;
    }

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    // Block 3's EIP-8130 batch is dropped and the slot is force-included as a
    // deposit-only block, whose state root differs from the sequencer's block 3.
    // Overwrite the sequencer's registered state root so the engine skips the
    // cross-check for the divergent slot.
    node.register_block_hash(3, eip8130_block_hash);
    node.initialize().await;

    // Blocks 1-2 derive from their valid batches; block 3's EIP-8130 batch is
    // dropped and stays pending while the epoch-0 window is still open.
    node.run_until_idle().await;
    assert_eq!(
        node.l2_safe_number(),
        2,
        "blocks 1-2 derive; block 3's EIP-8130 batch is dropped pre-Zenith"
    );

    // Close the epoch-0 sequencing window (0 + seq_window_size 4 = 4) so the
    // pipeline force-includes L2 slot 3 as a deposit-only block.
    h.l1.mine_block();
    chain.push(h.l1.tip().clone());
    node.run_until_idle().await;

    // The pipeline force-includes slot 3 as a deposit-only block and continues
    // deriving deposit-only blocks for the remaining slots in the available L1
    // range, so the safe head advances past block 3 (to 5 here). The exact
    // over-derivation count is incidental; this test's precise assertion is that
    // block 3 is deposit-only (below). Mirrors `upgrade/operator_fees.rs`.
    assert!(
        node.l2_safe_number() >= 3,
        "safe head must advance past block 3 via a deposit-only block"
    );

    let counts = node.derived_user_tx_counts();
    let user_txs = |n: u64| counts.iter().find(|(bn, _)| *bn == n).map(|(_, c)| *c);
    assert_eq!(user_txs(1), Some(1), "block 1: valid batch, 1 user tx");
    assert_eq!(user_txs(2), Some(1), "block 2: valid batch, 1 user tx");
    assert_eq!(
        user_txs(3),
        Some(0),
        "block 3: EIP-8130 batch dropped pre-Zenith -> deposit-only, 0 user txs"
    );
}
