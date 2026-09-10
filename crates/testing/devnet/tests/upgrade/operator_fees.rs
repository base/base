//! Action tests for operator fee encoding and upgrade activation.

use alloy_primitives::Address;
use base_batcher_encoding::{DaType, EncoderConfig};
use base_testing_devnet::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TestRollupConfigBuilder,
};

/// Build a `ConfigUpdate` log encoding an `OperatorFee` (type 5) change.
///
/// An `OperatorFee` system-config update committed to L1 is reflected in the
/// L1 info deposit transactions of derived L2 blocks once the L1 epoch advances
/// to the block containing the update.
///
/// `StatefulAttributesBuilder` re-reads the system config from the L2 provider
/// on every block, then additionally calls `update_with_receipts()` on the
/// freshly-fetched config only when the L2 epoch changes (i.e., the L2 block's
/// L1 origin differs from its parent's L1 origin). This means a `ConfigUpdate`
/// log in L1 block N is invisible to the attributes builder until the first L2
/// block whose epoch advances to N.
///
/// With L1 `block_time=12` s and L2 `block_time=2` s, six L2 blocks fit in one L1
/// epoch (genesis counts as block 0). The sequencer advances the epoch when
/// `next_l1.timestamp <= next_l2.timestamp`. L1 block 1 has ts=12, so the
/// epoch transitions at L2 block 6 (ts=12):
///
/// ```text
///   Pre-mined:
///     L1 block 1 (ts=12): OperatorFee update log
///   L2 blocks (L2 block_time = 2 s):
///     Block 0 – genesis    (ts= 0) epoch 0  – genesis state, not derived
///     Block 1  (ts= 2)     epoch 0  – OLD fee params
///     Block 2  (ts= 4)     epoch 0  – OLD fee params
///     Block 3  (ts= 6)     epoch 0  – OLD fee params
///     Block 4  (ts= 8)     epoch 0  – OLD fee params
///     Block 5  (ts=10)     epoch 0  – OLD fee params
///     Block 6  (ts=12)     epoch 1  ← epoch change: reads L1 block 1 receipts
///                                     → NEW fee params applied
///   Batched in L1:
///     L1 block 2: batches for L2 blocks 1–5
///     L1 block 3: batch  for L2 block 6
/// ```
///
/// Configuration: all forks through Isthmus are active at genesis so the
/// Isthmus-format L1 info deposit (which includes operator fee fields) is
/// used from the first block. Jovian is intentionally absent.
#[tokio::test]
async fn operator_fee_config_update_propagates_to_l1_info() {
    const OLD_SCALAR: u32 = 1_000;
    const OLD_CONSTANT: u64 = 500;
    const NEW_SCALAR: u32 = 3_000;
    const NEW_CONSTANT: u64 = 700;

    let l1_sys_cfg_addr = Address::repeat_byte(0xCC);
    let batcher_cfg = BatcherConfig::default();
    let mut rollup_cfg =
        TestRollupConfigBuilder::base_mainnet(&batcher_cfg).through_isthmus().build();
    rollup_cfg.l1_system_config_address = l1_sys_cfg_addr;
    let sys_cfg = rollup_cfg.genesis.system_config.as_mut().unwrap();
    sys_cfg.operator_fee_scalar = Some(OLD_SCALAR);
    sys_cfg.operator_fee_constant = Some(OLD_CONSTANT);

    // Standard L1 block_time (12 s). With L2 block_time=2 s, six L2 blocks
    // fit in each L1 epoch. The epoch change from 0 to 1 occurs at L2 block 6
    // (ts=12), when ts(L1 block 1)=12 ≤ ts(L2 block 6)=12.
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    // Pre-mine L1 block 1 (ts=12) with the OperatorFee update so the sequencer
    // can reference epoch 1 when it builds L2 block 6.
    h.l1.enqueue_operator_fee_update(l1_sys_cfg_addr, NEW_SCALAR, NEW_CONSTANT);
    h.l1.mine_block(); // L1 block 1, ts=12

    // Snapshot the chain (blocks 0 and 1) before building L2 blocks. The sequencer
    // needs L1 block 1 in its chain to advance the epoch at L2 block 6.
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // L2 blocks 1–5 (ts=2,4,6,8,10): epoch 0, OLD config.
    let mut epoch0_blocks: Vec<base_common_types_chain::BaseBlock> = Vec::new();
    for _ in 0..5 {
        let block = sequencer.build_next_block_with_single_transaction().await;
        epoch0_blocks.push(block);
    }

    // L2 block 6 (ts=12): epoch 1, epoch change — NEW config from L1 block 1's receipts.
    let block6 = sequencer.build_next_block_with_single_transaction().await;

    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..batcher_cfg.encoder.clone() },
        ..batcher_cfg
    };

    // Batch all epoch-0 blocks into L1 block 2 (one Batcher with all 5 blocks).
    {
        let mut source = ActionL2Source::new();
        for block in epoch0_blocks {
            source.push(block);
        }
        let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
        batcher.advance(&mut h.l1).await; // L1 block 2, ts=24
    }

    // Batch block 6 into L1 block 3.
    {
        let mut source = ActionL2Source::new();
        source.push(block6);
        let mut batcher = Batcher::new(source, &h.rollup_config, batcher_cfg.clone());
        batcher.advance(&mut h.l1).await; // L1 block 3, ts=36
    }

    // Node snapshot includes all L1 blocks 0–3.
    let (mut node, _chain) = h.create_test_rollup_node_from_sequencer(
        &mut sequencer,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );
    node.initialize().await;

    for _ in 1u64..=3 {
        node.run_until_idle().await;
    }

    assert_eq!(node.l2_safe_number(), 6, "all 6 L2 blocks must be derived");

    let infos = node.derived_l1_info_txs();
    let find = |n: u64| infos.iter().find(|(bn, _)| *bn == n).map(|(_, tx)| tx);

    // Blocks 1–5 (epoch 0, no receipt update) carry OLD fee params.
    for n in 1u64..=5 {
        let info = find(n).unwrap_or_else(|| panic!("L1 info tx for block {n} must be recorded"));
        assert_eq!(
            info.operator_fee_scalar(),
            OLD_SCALAR,
            "block {n}: operator_fee_scalar must reflect the genesis SystemConfig"
        );
        assert_eq!(
            info.operator_fee_constant(),
            OLD_CONSTANT,
            "block {n}: operator_fee_constant must reflect the genesis SystemConfig"
        );
    }

    // Block 6 (epoch 1 — first epoch change) carries NEW fee params. This is the
    // "seventh" block total counting from genesis (block 0), confirming that
    // StatefulAttributesBuilder reads L1 block 1's receipts on the epoch change.
    let info6 = find(6).expect("L1 info tx for block 6 must be recorded");
    assert_eq!(
        info6.operator_fee_scalar(),
        NEW_SCALAR,
        "block 6: operator_fee_scalar must reflect the OperatorFee config update"
    );
    assert_eq!(
        info6.operator_fee_constant(),
        NEW_CONSTANT,
        "block 6: operator_fee_constant must reflect the OperatorFee config update"
    );
}
