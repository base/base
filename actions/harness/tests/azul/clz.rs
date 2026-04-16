//! CLZ opcode activation test across the Base Azul boundary.

use alloy_primitives::{Bytes, TxKind, U256, hex};
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TEST_ACCOUNT_ADDRESS, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// CLZ probe-contract init code.
///
/// Deploys runtime that:
///  1. `CALLDATALOAD(0) → DUP → CLZ → SSTORE(slot 0)` — stores the CLZ result.
///  2. `GAS → SWAP → CLZ → POP → GAS → SWAP → SUB → SSTORE(slot 2)` — stores CLZ gas delta.
///  3. `PUSH 1 → SSTORE(slot 1)` — sentinel proving execution completed.
///
/// If CLZ aborts (pre-fork, invalid opcode) no SSTORE executes.
const CLZ_INIT_CODE: [u8; 36] =
    hex!("6018600c60003960186000f3600035801e6000555a901e505a9003600255600160015500");

/// Input word `1` — `CLZ(1) = 255`.
const CLZ_INPUT_ONE: [u8; 32] =
    hex!("0000000000000000000000000000000000000000000000000000000000000001");

/// Input word with the high bit set — `CLZ(0x8000…0) = 0`.
const CLZ_INPUT_HIGH_BIT: [u8; 32] =
    hex!("8000000000000000000000000000000000000000000000000000000000000000");

/// Storage slot where the CLZ result is written.
const CLZ_RESULT_SLOT: U256 = U256::ZERO;

/// Storage slot where the post-CLZ sentinel (`1`) is written.
const CLZ_SENTINEL_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Storage slot where the measured gas delta is written.
const CLZ_GAS_DELTA_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

/// Expected gas delta between the two `GAS` readings around CLZ.
///
/// The measured window includes `SWAP1(3) + CLZ(5) + POP(2) + GAS(2) = 12`.
const CLZ_EXPECTED_GAS_DELTA: u64 = 12;

#[tokio::test]
async fn azul_clz_op_code() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..Default::default()
    };

    // All forks through Jovian at genesis; Base Azul at ts=6 (block 3).
    let base_azul_time = 6u64;
    let rollup_cfg = TestRollupConfigBuilder::base_mainnet(&batcher_cfg)
        .through_isthmus()
        .with_jovian_at(0)
        .with_azul_at(base_azul_time)
        .build();
    let chain_id = rollup_cfg.l2_chain_id.id();
    let mut h = ActionTestHarness::new(L1MinerConfig::default(), rollup_cfg);

    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut builder = h.create_l2_sequencer(l1_chain);

    let (mut node, chain) = h.create_test_rollup_node_from_sequencer(
        &mut builder,
        SharedL1Chain::from_blocks(h.l1.chain().to_vec()),
    );

    let account = builder.test_account();
    let contract_addr = TEST_ACCOUNT_ADDRESS.create(0);

    // ── Block 1 (ts=2, pre-fork): deploy CLZ probe contract ──────────
    let deploy_tx = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Create,
            Bytes::from_static(&CLZ_INIT_CODE),
            U256::ZERO,
            100_000,
        )
    };
    let block1 = builder.build_next_block_with_transactions(vec![deploy_tx]).await;

    // Verify the contract code was deployed.
    assert!(builder.has_code(contract_addr), "deployed contract must have non-empty code");

    // ── Block 2 (ts=4, pre-fork): call CLZ(1) — must abort ──────────
    let call_pre = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Call(contract_addr),
            Bytes::from_static(&CLZ_INPUT_ONE),
            U256::ZERO,
            100_000,
        )
    };
    let block2 = builder.build_next_block_with_transactions(vec![call_pre]).await;

    // Sentinel slot must remain zero — CLZ aborted before any SSTORE ran.
    assert_eq!(
        builder.storage_at(contract_addr, CLZ_SENTINEL_SLOT),
        U256::ZERO,
        "sentinel must be zero: CLZ should abort as invalid opcode pre-fork"
    );

    // ── Block 3 (ts=6, post-fork): call CLZ(1) — must succeed ───────
    let call_one = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Call(contract_addr),
            Bytes::from_static(&CLZ_INPUT_ONE),
            U256::ZERO,
            100_000,
        )
    };
    let block3 = builder.build_next_block_with_transactions(vec![call_one]).await;

    // Sentinel must now be 1 (CLZ completed), result slot must be 255.
    {
        let sentinel = builder.storage_at(contract_addr, CLZ_SENTINEL_SLOT);
        let result = builder.storage_at(contract_addr, CLZ_RESULT_SLOT);
        let gas_delta = builder.storage_at(contract_addr, CLZ_GAS_DELTA_SLOT);
        assert_eq!(sentinel, U256::from(1), "sentinel must be 1 after successful CLZ");
        assert_eq!(result, U256::from(255), "CLZ(1) must equal 255");
        assert_eq!(
            gas_delta,
            U256::from(CLZ_EXPECTED_GAS_DELTA),
            "gas delta must be {CLZ_EXPECTED_GAS_DELTA} (SWAP1=3 + CLZ=5 + POP=2 + GAS=2)"
        );
    }

    // ── Block 4 (ts=8, post-fork): call CLZ(0x8000…0) — result = 0 ──
    let call_high = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Call(contract_addr),
            Bytes::from_static(&CLZ_INPUT_HIGH_BIT),
            U256::ZERO,
            100_000,
        )
    };
    let block4 = builder.build_next_block_with_transactions(vec![call_high]).await;

    {
        let sentinel = builder.storage_at(contract_addr, CLZ_SENTINEL_SLOT);
        let result = builder.storage_at(contract_addr, CLZ_RESULT_SLOT);
        let gas_delta = builder.storage_at(contract_addr, CLZ_GAS_DELTA_SLOT);
        assert_eq!(sentinel, U256::from(1), "sentinel must remain 1");
        assert_eq!(result, U256::ZERO, "CLZ(0x8000…0) must equal 0");
        assert_eq!(
            gas_delta,
            U256::from(CLZ_EXPECTED_GAS_DELTA),
            "gas delta must be consistent across inputs"
        );
    }

    // ── Batch and derive all 4 blocks ────────────────────────────────
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    node.initialize().await;

    for (block, i) in [(block1, 1u64), (block2, 2), (block3, 3), (block4, 4)] {
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
        let derived = node.run_until_idle().await;
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");
    }

    assert_eq!(
        node.l2_safe().block_info.number,
        4,
        "all 4 L2 blocks must derive through the Base V1 boundary"
    );
}
