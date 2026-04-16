//! MODEXP precompile tests across the Base Azul boundary.

use alloy_primitives::{Bytes, TxKind, U256, hex};
use base_action_harness::{
    ActionL2Source, ActionTestHarness, Batcher, BatcherConfig, L1MinerConfig, SharedL1Chain,
    TEST_ACCOUNT_ADDRESS, TestRollupConfigBuilder,
};
use base_batcher_encoder::{DaType, EncoderConfig};

// ─── MODEXP probe contract ──────────────────────────────────────────
//
// Deploys runtime that:
//  1. Copies calldata into memory (the raw MODEXP precompile input).
//  2. Records `GAS` before the `STATICCALL`.
//  3. `STATICCALL(gas, 0x05, 0, calldatasize, calldatasize, 32)` — forwards
//     calldata to the MODEXP precompile.
//  4. Records `GAS` after the `STATICCALL`.
//  5. `SSTORE(slot 0, success)` — 1 if the call succeeded, 0 otherwise.
//  6. `SSTORE(slot 1, gas_before − gas_after)` — total gas delta.
//  7. `SSTORE(slot 2, 1)` — sentinel proving execution completed.

/// MODEXP probe-contract init code (12 bytes init + 33 bytes runtime).
///
/// Runtime bytecode:
/// ```text
/// CALLDATASIZE PUSH1 0 PUSH1 0 CALLDATACOPY       ; mem[0..cds] = calldata
/// GAS                                               ; gas_before
/// PUSH1 0x20 CALLDATASIZE CALLDATASIZE PUSH1 0      ; retSz retOff argSz argOff
/// PUSH1 0x05 GAS STATICCALL                         ; success
/// GAS                                               ; gas_after
/// SWAP1 PUSH1 0 SSTORE                              ; slot 0 = success
/// SWAP1 SUB PUSH1 1 SSTORE                          ; slot 1 = gas_before - gas_after
/// PUSH1 1 PUSH1 2 SSTORE                            ; slot 2 = 1 (sentinel)
/// STOP
/// ```
const MODEXP_INIT_CODE: [u8; 45] = hex!(
    "6021600c60003960216000f3"   // init: CODECOPY 33 bytes from offset 12, RETURN
    "3660006000375a"             // runtime: CALLDATACOPY + GAS(before)
    "60203636600060055afa"       // STATICCALL(gas, 0x05, 0, cds, cds, 32)
    "5a"                         // GAS(after)
    "9060005590036001556001600255" // SSTOREs: slot0=success, slot1=delta, slot2=sentinel
    "00"                         // STOP
);

/// Storage slot where the STATICCALL success flag is written (1 = success, 0 = revert).
const MODEXP_SUCCESS_SLOT: U256 = U256::ZERO;

/// Storage slot where the measured gas delta is written.
const MODEXP_GAS_DELTA_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Storage slot where the sentinel value (`1`) is written.
const MODEXP_SENTINEL_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

/// Build a raw MODEXP precompile input with the given field sizes and data.
///
/// Format: `[base_len (32B) | exp_len (32B) | mod_len (32B) | base | exponent | modulus]`.
fn modexp_input(base: &[u8], exponent: &[u8], modulus: &[u8]) -> Vec<u8> {
    let mut input = Vec::new();
    // base_len
    input.extend_from_slice(&U256::from(base.len()).to_be_bytes::<32>());
    // exp_len
    input.extend_from_slice(&U256::from(exponent.len()).to_be_bytes::<32>());
    // mod_len
    input.extend_from_slice(&U256::from(modulus.len()).to_be_bytes::<32>());
    // data
    input.extend_from_slice(base);
    input.extend_from_slice(exponent);
    input.extend_from_slice(modulus);
    input
}

/// EIP-7823: MODEXP rejects inputs with any field length > 1024 bytes after Base Azul.
///
/// Pre-fork the oversized call succeeds; post-fork it fails.
#[tokio::test]
async fn azul_modexp_upper_bound() {
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

    // ── Block 1 (ts=2, pre-fork): deploy MODEXP probe contract ──────
    let deploy_tx = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Create,
            Bytes::from_static(&MODEXP_INIT_CODE),
            U256::ZERO,
            100_000,
        )
    };
    let block1 = builder.build_next_block_with_transactions(vec![deploy_tx]).await;

    assert!(builder.has_code(contract_addr), "deployed contract must have non-empty code");

    // Oversized input: base_len = 1025 (> 1024-byte EIP-7823 limit).
    let oversized_input = modexp_input(&vec![0u8; 1025], &[], &[2]);

    // ── Block 2 (ts=4, pre-fork): call MODEXP with oversized input ───
    let call_pre = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Call(contract_addr),
            Bytes::from(oversized_input.clone()),
            U256::ZERO,
            1_000_000,
        )
    };
    let block2 = builder.build_next_block_with_transactions(vec![call_pre]).await;

    // Pre-fork: oversized MODEXP succeeds.
    {
        let sentinel = builder.storage_at(contract_addr, MODEXP_SENTINEL_SLOT);
        let success = builder.storage_at(contract_addr, MODEXP_SUCCESS_SLOT);
        assert_eq!(sentinel, U256::from(1), "sentinel must be 1: probe completed pre-fork");
        assert_eq!(success, U256::from(1), "MODEXP with oversized input must succeed pre-fork");
    }

    // ── Block 3 (ts=6, post-fork): call MODEXP with oversized input ──
    let call_post = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Call(contract_addr),
            Bytes::from(oversized_input),
            U256::ZERO,
            1_000_000,
        )
    };
    let block3 = builder.build_next_block_with_transactions(vec![call_post]).await;

    // Post-fork: oversized MODEXP must fail (EIP-7823).
    {
        let success = builder.storage_at(contract_addr, MODEXP_SUCCESS_SLOT);
        assert_eq!(
            success,
            U256::ZERO,
            "MODEXP with oversized input must fail post-fork (EIP-7823)"
        );
    }

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

/// EIP-7883: MODEXP gas cost increases after Base Azul (min 200→500, general cost tripled).
#[tokio::test]
async fn azul_modexp_gas_cost_increase() {
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

    // ── Block 1 (ts=2, pre-fork): deploy MODEXP probe contract ──────
    let deploy_tx = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Create,
            Bytes::from_static(&MODEXP_INIT_CODE),
            U256::ZERO,
            100_000,
        )
    };
    let block1 = builder.build_next_block_with_transactions(vec![deploy_tx]).await;

    assert!(builder.has_code(contract_addr), "deployed contract must have non-empty code");

    // Small valid input: 2^3 mod 5 (= 3).
    let small_input = modexp_input(&[2], &[3], &[5]);

    // ── Block 2 (ts=4, pre-fork): call MODEXP ────────────────────────
    let call_pre = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Call(contract_addr),
            Bytes::from(small_input.clone()),
            U256::ZERO,
            100_000,
        )
    };
    let block2 = builder.build_next_block_with_transactions(vec![call_pre]).await;

    let gas_delta_pre;
    {
        let sentinel = builder.storage_at(contract_addr, MODEXP_SENTINEL_SLOT);
        let success = builder.storage_at(contract_addr, MODEXP_SUCCESS_SLOT);
        gas_delta_pre = builder.storage_at(contract_addr, MODEXP_GAS_DELTA_SLOT);
        assert_eq!(sentinel, U256::from(1), "sentinel must be 1: probe completed pre-fork");
        assert_eq!(success, U256::from(1), "MODEXP must succeed pre-fork");
    }

    // ── Block 3 (ts=6, post-fork): call MODEXP with same input ───────
    let call_post = {
        let mut acct = account.lock().expect("test account lock");
        acct.create_tx(
            chain_id,
            TxKind::Call(contract_addr),
            Bytes::from(small_input),
            U256::ZERO,
            100_000,
        )
    };
    let block3 = builder.build_next_block_with_transactions(vec![call_post]).await;

    let gas_delta_post;
    {
        let success = builder.storage_at(contract_addr, MODEXP_SUCCESS_SLOT);
        gas_delta_post = builder.storage_at(contract_addr, MODEXP_GAS_DELTA_SLOT);
        assert_eq!(success, U256::from(1), "MODEXP must succeed post-fork");
    }

    // EIP-7883 raises the minimum gas cost from 200 to 500 and triples the general
    // cost, so the post-fork delta must be strictly larger than the pre-fork delta.
    assert!(
        gas_delta_post > gas_delta_pre,
        "post-fork MODEXP gas delta ({gas_delta_post}) must exceed pre-fork delta \
         ({gas_delta_pre}) due to EIP-7883 cost increase"
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
        "all 3 L2 blocks must derive through the Base V1 boundary"
    );
}
