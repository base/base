//! P256VERIFY precompile gas cost test across the Base Azul boundary.

use alloy_primitives::{Bytes, TxKind, U256, hex};
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TEST_ACCOUNT_ADDRESS, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

/// P256VERIFY probe-contract init code (12 bytes init + 34 bytes runtime).
///
/// Identical to the MODEXP probe except the STATICCALL target is `PUSH2 0x0100`
/// (RIP-7212 P256VERIFY address) instead of `PUSH1 0x05`.
const P256_INIT_CODE: [u8; 46] = hex!(
    "6022600c60003960226000f3"     // init: CODECOPY 34 bytes from offset 12, RETURN
    "3660006000375a"               // runtime: CALLDATACOPY + GAS(before)
    "602036366000610100"           // retSz retOff argSz argOff PUSH2(0x0100)
    "5afa"                         // GAS STATICCALL
    "5a"                           // GAS(after)
    "9060005590036001556001600255"  // SSTOREs: slot0=success, slot1=delta, slot2=sentinel
    "00"                           // STOP
);

/// Storage slot where the P256 STATICCALL success flag is written.
const P256_SUCCESS_SLOT: U256 = U256::ZERO;

/// Storage slot where the P256 measured gas delta is written.
const P256_GAS_DELTA_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Storage slot where the P256 sentinel value (`1`) is written.
const P256_SENTINEL_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

/// P256VERIFY gas cost doubles after Base Azul (3,450 → 6,900).
#[tokio::test]
async fn azul_p256_verify_gas_cost_increase() {
    let batcher_cfg = BatcherConfig {
        encoder: EncoderConfig { da_type: DaType::Calldata, ..EncoderConfig::default() },
        ..Default::default()
    };

    // Base Azul activates at ts=6 (block 3).
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

    // ── Block 1 (ts=2, pre-fork): deploy P256VERIFY probe contract ───
    let deploy_tx = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Create,
            Bytes::from_static(&P256_INIT_CODE),
            U256::ZERO,
            100_000,
        )
    };
    let block1 = builder.build_next_block_with_transactions(vec![deploy_tx]).await;

    assert!(builder.has_code(contract_addr), "deployed contract must have non-empty code");

    // Empty calldata — the precompile returns empty output (invalid sig) but
    // still charges its base gas fee, which is what we measure.
    let p256_input = Bytes::new();

    // ── Block 2 (ts=4, pre-fork): call P256VERIFY ────────────────────
    let call_pre = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Call(contract_addr),
            p256_input.clone(),
            U256::ZERO,
            100_000,
        )
    };
    let block2 = builder.build_next_block_with_transactions(vec![call_pre]).await;

    let gas_delta_pre;
    {
        let sentinel = builder.storage_at(contract_addr, P256_SENTINEL_SLOT);
        let success = builder.storage_at(contract_addr, P256_SUCCESS_SLOT);
        gas_delta_pre = builder.storage_at(contract_addr, P256_GAS_DELTA_SLOT);
        assert_eq!(sentinel, U256::from(1), "sentinel must be 1: probe completed pre-fork");
        assert_eq!(success, U256::from(1), "P256VERIFY must succeed pre-fork");
    }

    // ── Block 3 (ts=6, post-fork): call P256VERIFY with same input ───
    let call_post = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(chain_id, TxKind::Call(contract_addr), p256_input, U256::ZERO, 100_000)
    };
    let block3 = builder.build_next_block_with_transactions(vec![call_post]).await;

    let gas_delta_post;
    {
        let success = builder.storage_at(contract_addr, P256_SUCCESS_SLOT);
        gas_delta_post = builder.storage_at(contract_addr, P256_GAS_DELTA_SLOT);
        assert_eq!(success, U256::from(1), "P256VERIFY must succeed post-fork");
    }

    // The base gas fee doubles from 3,450 to 6,900 at Base Azul.
    assert!(
        gas_delta_post > gas_delta_pre,
        "post-fork P256VERIFY gas delta ({gas_delta_post}) must exceed pre-fork delta \
         ({gas_delta_pre}) due to doubled base gas fee"
    );

    // ── Batch and derive ─────────────────────────────────────────────
    let mut batcher = Batcher::new(ActionL2Source::new(), &h.rollup_config, batcher_cfg.clone());
    node.initialize().await;

    for (block, i) in [(block1, 1u64), (block2, 2), (block3, 3)] {
        batcher.push_block(block);
        batcher.advance(&mut h.l1).await;
        chain.push(h.l1.tip().clone());
        let derived = node.run_until_idle().await;
        assert_eq!(derived, 1, "L1 block {i} should derive exactly one L2 block");
    }

    assert_eq!(
        node.l2_safe().block_info.number,
        3,
        "all 3 L2 blocks must derive through the Base Azul boundary"
    );
}
