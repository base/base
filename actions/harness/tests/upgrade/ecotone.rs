//! Action tests for the Ecotone upgrade activation boundary.

use base_action_harness::{
    ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};
use base_common_genesis::UpgradeConfig;
use base_protocol::L1BlockInfoTx;
use rstest::rstest;

// ---------------------------------------------------------------------------
// A. L1 info format transitions at Ecotone activation
// ---------------------------------------------------------------------------

/// The L1 info deposit transaction changes format at Ecotone activation in
/// **two steps**:
///
/// 1. Before Ecotone: `L1BlockInfoTx::Bedrock` (no blob base fee, 4-byte
///    `setL1BlockValues` selector).
/// 2. At the **first** Ecotone block: still `Bedrock` format, because the
///    `L1Block` contract has not yet been upgraded (upgrade transactions are
///    placed *after* the L1 info deposit, so the contract is still on the old
///    ABI for the first block).
/// 3. From the **second** Ecotone block onward: `L1BlockInfoTx::Ecotone`
///    (with `blob_base_fee`, `blob_base_fee_scalar`, new selector).
///
/// `operator_fees.rs` tests the Isthmus→Jovian transition; this covers the
/// earlier pre-Ecotone → Ecotone boundary.
#[tokio::test]
async fn ecotone_l1_info_format_transitions_at_activation() {
    let batcher_cfg = BatcherConfig::default();

    // Canyon and Delta active at genesis; Ecotone activates at ts=6 (block 3,
    // block_time=2). All earlier forks silent so they don't interfere.
    let ecotone_time = 6u64;
    let upgrades = UpgradeConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(ecotone_time),
        ..Default::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_upgrades(upgrades).build();
    let h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    // Block 1: ts=2 — pre-Ecotone. Expect Bedrock format.
    let block1 = builder.build_next_block_with_single_transaction().await;
    let info1 = ActionTestHarness::l1_info_from_block(&block1);
    assert!(
        matches!(info1, L1BlockInfoTx::Bedrock(_)),
        "block 1 (pre-Ecotone ts=2) must use Bedrock format, got {info1:?}"
    );

    // Block 2: ts=4 — still pre-Ecotone. Expect Bedrock format.
    let block2 = builder.build_next_block_with_single_transaction().await;
    let info2 = ActionTestHarness::l1_info_from_block(&block2);
    assert!(
        matches!(info2, L1BlockInfoTx::Bedrock(_)),
        "block 2 (pre-Ecotone ts=4) must use Bedrock format, got {info2:?}"
    );

    // Block 3: ts=6 — FIRST Ecotone block. Protocol rule: L1Block contract
    // upgrade tx is appended AFTER the L1 info deposit, so the contract is
    // still on the Bedrock ABI. The sequencer sends a Bedrock-format L1 info tx.
    let block3 = builder.build_empty_block().await;
    assert_eq!(block3.header.timestamp, ecotone_time, "block 3 must be at ecotone_time");
    let info3 = ActionTestHarness::l1_info_from_block(&block3);
    assert!(
        matches!(info3, L1BlockInfoTx::Bedrock(_)),
        "block 3 (first Ecotone ts=6) must still use Bedrock format, got {info3:?}"
    );

    // Block 4: ts=8 — second Ecotone block. L1Block contract now upgraded.
    // Sequencer sends Ecotone-format L1 info tx.
    let block4 = builder.build_next_block_with_single_transaction().await;
    let info4 = ActionTestHarness::l1_info_from_block(&block4);
    assert!(
        matches!(info4, L1BlockInfoTx::Ecotone(_)),
        "block 4 (post-Ecotone ts=8) must use Ecotone format, got {info4:?}"
    );
}

// ---------------------------------------------------------------------------
// B. Derivation through the Ecotone activation boundary
// ---------------------------------------------------------------------------

/// Derivation succeeds across the Ecotone activation boundary (ts=6, L2 block 3).
///
/// Unlike Jovian, Ecotone has no `NonEmptyTransitionBlock` batch check, so the
/// activation block's batch is accepted whether it contains user transactions or
/// is empty. `operator_fees.rs` covers the Isthmus→Jovian boundary where the
/// check does fire.
#[rstest]
#[case::activation_block_with_user_tx(true)]
#[case::empty_activation_block(false)]
#[tokio::test]
async fn ecotone_derivation_crosses_activation_boundary(#[case] with_user_tx: bool) {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..BatcherConfig::default()
    };

    let ecotone_time = 6u64;
    let upgrades = UpgradeConfig {
        canyon_time: Some(0),
        delta_time: Some(0),
        ecotone_time: Some(ecotone_time),
        ..Default::default()
    };
    let rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).with_upgrades(upgrades).build();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    for i in 1..=4u64 {
        let block = if i == 3 && !with_user_tx {
            builder.build_empty_block().await
        } else {
            builder.build_next_block_with_single_transaction().await
        };
        if i == 3 {
            assert_eq!(
                block.header.timestamp, ecotone_time,
                "block 3 must land exactly at ecotone_time"
            );
        }
        h.submit_single_batch_zlib_calldata(&batcher_cfg, &block, i - 1)
            .expect("valid zlib batch across Ecotone activation");
    }

    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    let total_derived = node.run_until_idle().await;
    assert_eq!(total_derived, 4, "all 4 L2 blocks must be derived");
    assert_eq!(
        node.l2_safe_number(),
        4,
        "derivation must succeed through the Ecotone activation boundary"
    );
}
