//! Regression test: block #43127820 derivation-pipeline payload attributes bug.
//!
//! # What this test verifies
//!
//! This test uses **real chain data** fetched from Base mainnet and Ethereum mainnet to
//! reproduce the exact conditions under which `StatefulAttributesBuilder::prepare_payload_attributes`
//! generates an incorrect L1BlockInfo deposit transaction for Base mainnet block #43127820,
//! causing a state-root mismatch.
//!
//! The test runs the derivation pipeline with the production `L1Config::mainnet()` and the
//! real `SystemConfig` state for block #43127819 (the L2 parent), then compares the pipeline
//! output against the actual values observed in block #43127820's L1BlockInfo deposit.
//!
//! ## Bug: `da_footprint_gas_scalar`
//!
//! At the time block #43127820 was derived, the proof pipeline's `SystemConfig` for the L2
//! parent (block #43127819) had `da_footprint_gas_scalar: None` — the
//! `DaFootprintGasScalarUpdate` L1 event had not been surfaced.  `L1BlockInfoTx::try_new`
//! falls back to `L1BlockInfoJovian::DEFAULT_DA_FOOTPRINT_GAS_SCALAR (= 400)` instead of
//! the correct on-chain value (139).
//!
//! # How to interpret results
//!
//! * **FAIL** on `da_footprint_gas_scalar` assertion → bug is present (expected state until
//!   the underlying issue is fixed).
//! * **PASS** on all assertions → bug has been fixed; this test now acts as a regression guard.

use std::sync::Arc;

use alloy_consensus::Header;
use alloy_eips::{BlockNumHash, eip2718::Decodable2718};
use alloy_primitives::{B256, U256, address, b256};
use base_alloy_consensus::OpTxEnvelope;
use base_consensus_derive::{
    AttributesBuilder, StatefulAttributesBuilder,
    test_utils::{TestChainProvider, TestSystemConfigL2Fetcher},
};
use base_consensus_genesis::{HardForkConfig, L1ChainConfig, RollupConfig, SystemConfig};
use base_consensus_registry::L1Config;
use base_protocol::{BlockInfo, L1BlockInfoJovian, L1BlockInfoTx, L2BlockInfo};

// ── Real chain data constants ──────────────────────────────────────────────────────────────

/// Ethereum mainnet block number that is the shared L1 origin of L2 blocks #43127819 and
/// #43127820. Both blocks share the same epoch, so derivation uses the same-epoch code path.
const L1_BLOCK_NUMBER: u64 = 24_618_713;

/// Real `timestamp` from Ethereum mainnet block #24618713 (seconds since Unix epoch).
const L1_BLOCK_TIMESTAMP: u64 = 1_773_044_795;

/// Real `baseFeePerGas` (in wei) from Ethereum mainnet block #24618713.
const L1_BASE_FEE: u64 = 38_369_591;

/// Real `excessBlobGas` from Ethereum mainnet block #24618713.
/// With BPO2 params this yields a blob base fee of `EXPECTED_BLOB_BASE_FEE`.
const L1_EXCESS_BLOB_GAS: u64 = 170_294_236;

/// Real block number of the L2 parent (the block immediately preceding #43127820).
const L2_PARENT_NUMBER: u64 = 43_127_819;

/// Real `timestamp` from Base mainnet block #43127819.
const L2_PARENT_TIMESTAMP: u64 = 1_773_044_985;

/// Real block hash of Base mainnet block #43127819.
const L2_PARENT_HASH: B256 =
    b256!("fa72b5be228a5fa90a39fc065ccd8a91bd070f13617f1d0cff8ff6bb6548f1e3");

/// Real `seq_num` from block #43127819's L1BlockInfo deposit.
/// Block #43127820 must encode `seq_num = L2_PARENT_SEQ_NUM + 1 = 4`.
const L2_PARENT_SEQ_NUM: u64 = 3;

/// Expected `sequence_number` in block #43127820's L1BlockInfo deposit (parent + 1).
const EXPECTED_SEQ_NUM: u64 = L2_PARENT_SEQ_NUM + 1;

/// Expected `blob_base_fee` in block #43127820's L1BlockInfo deposit.
///
/// Computed as `BlobParams::bpo2().calc_blob_fee(excess_blob_gas=170_294_236)`.
/// This is the value actually present in block #43127820 on-chain.
const EXPECTED_BLOB_BASE_FEE: u128 = 2_135_385;

/// Real `da_footprint_gas_scalar` decoded from block #43127820's L1BlockInfo deposit.
///
/// The proof pipeline had `SystemConfig.da_footprint_gas_scalar = None` (the
/// `DaFootprintGasScalarUpdate` L1 event had not been processed), so it fell back to
/// `DEFAULT_DA_FOOTPRINT_GAS_SCALAR = 400` instead of this correct on-chain value.
const EXPECTED_DA_FOOTPRINT_GAS_SCALAR: u16 = 139;

// ── Helpers ───────────────────────────────────────────────────────────────────────────────

/// Build a [`RollupConfig`] with all hardforks (including Jovian) always active and
/// `block_time = 2`, matching Base mainnet.
fn make_rollup_config() -> Arc<RollupConfig> {
    Arc::new(RollupConfig {
        block_time: 2,
        hardforks: HardForkConfig {
            regolith_time: Some(1),
            canyon_time: Some(1),
            delta_time: Some(1),
            ecotone_time: Some(1),
            fjord_time: Some(1),
            granite_time: Some(1),
            holocene_time: Some(1),
            isthmus_time: Some(1),
            jovian_time: Some(1),
            pectra_blob_schedule_time: None,
            base: None,
        },
        ..Default::default()
    })
}

// ── Old hypothesis-driven tests (NOT grounded in real chain data) ─────────────────────────

/*
LIKELY UNTESTED / INVALID — not grounded in real chain data.

The expected value (250) was incorrect; the real on-chain da_footprint_gas_scalar decoded
from block #43127820 is 139, not 250.  The test infrastructure also used a synthetic L1
timestamp (1_773_000_000), a synthetic L2 parent with seq_num=5, and a synthetic L1 block
number (20_000_000), none of which reflect the production scenario.

See `test_block_43127820_real_chain_data` below for the correct regression test.

#[tokio::test]
async fn test_block_43127820_da_footprint_gas_scalar_bug() {
    let rollup_cfg = make_rollup_config();
    let l1_cfg: Arc<L1ChainConfig> = Arc::new(L1Config::mainnet().into());

    let l1_header = make_l1_header();
    let l1_hash = l1_header.hash_slow();

    let sys_config = SystemConfig { da_footprint_gas_scalar: None, ..Default::default() };

    let mut fetcher = TestSystemConfigL2Fetcher::default();
    fetcher.insert(L2_PARENT_NUMBER, sys_config);

    let mut provider = TestChainProvider::default();
    provider.insert_header(l1_hash, l1_header.clone());

    let (l2_parent, epoch) = make_l2_parent_and_epoch(l1_hash);

    let mut builder = StatefulAttributesBuilder::new(rollup_cfg, l1_cfg, fetcher, provider);
    let attrs = builder
        .prepare_payload_attributes(l2_parent, epoch)
        .await
        .expect("prepare_payload_attributes must succeed");

    let txs = attrs.transactions.expect("transactions must be present");
    let envelope =
        OpTxEnvelope::decode_2718(&mut txs[0].as_ref()).expect("valid 2718-encoded deposit");
    let OpTxEnvelope::Deposit(deposit) = envelope else {
        panic!("first tx must be a deposit");
    };
    let l1_info =
        L1BlockInfoTx::decode_calldata(&deposit.input).expect("calldata must be valid L1BlockInfo");
    let L1BlockInfoTx::Jovian(ref jovian) = l1_info else {
        panic!("expected Jovian L1BlockInfoTx, got {l1_info:?}");
    };

    // Wrong expected value (250 != 139). Not a valid regression test.
    assert_eq!(jovian.da_footprint_gas_scalar, 250);
}
*/

/*
LIKELY UNTESTED / INVALID — not grounded in real chain data.

The production proof system uses `L1Config::mainnet()` which already includes BPO2 in its
`blob_schedule`, so the empty-blob-schedule scenario does not represent production.  The
Prague timestamp (1_746_612_311) was an arbitrary placeholder, and the SystemConfig used a
synthetic `da_footprint_gas_scalar` value (250) that does not match the real on-chain value
(139).

See `test_block_43127820_real_chain_data` below for the correct regression test.

#[tokio::test]
async fn test_block_43127820_blob_fee_bug() {
    let rollup_cfg = make_rollup_config();

    let l1_cfg_without_bpo2 = L1ChainConfig {
        prague_time: Some(1_746_612_311),
        blob_schedule: Default::default(),
        ..Default::default()
    };
    let l1_cfg: Arc<L1ChainConfig> = Arc::new(l1_cfg_without_bpo2);

    // ... rest of test omitted ...
}
*/

// ── Real-data regression test ─────────────────────────────────────────────────────────────

/// Regression test for block #43127820 using **real on-chain data**.
///
/// # Chain data sources
///
/// * L1 header: Ethereum mainnet block #24618713 (the shared L1 origin of both #43127819 and
///   #43127820), fetched via `eth_getBlockByNumber`.
/// * L2 parent: Base mainnet block #43127819, decoded from `eth_getBlockByNumber` and its
///   first transaction (the L1BlockInfo deposit).
/// * `SystemConfig` scalars: decoded from block #43127820's L1BlockInfo deposit
///   (`base_fee_scalar=2269`, `blob_base_fee_scalar=1055762`).  `da_footprint_gas_scalar` is
///   set to `None` to simulate the production proof pipeline state where the
///   `DaFootprintGasScalarUpdate` event had not been processed for this block.
///
/// # Expected failure
///
/// The `da_footprint_gas_scalar` assertion fails with the current code because
/// `L1BlockInfoTx::try_new` defaults to `DEFAULT_DA_FOOTPRINT_GAS_SCALAR = 400` when
/// `SystemConfig.da_footprint_gas_scalar` is `None`, whereas the on-chain value is 139.
///
/// The `blob_base_fee` and `sequence_number` assertions pass because `L1Config::mainnet()`
/// correctly includes BPO2 in its `blob_schedule` and the same-epoch seq_num increment works
/// as expected.
#[tokio::test]
async fn test_block_43127820_real_chain_data() {
    let rollup_cfg = make_rollup_config();

    // The production proof system uses L1Config::mainnet(), which has BPO2 in blob_schedule.
    // `active_scheduled_params_at_timestamp(1_773_044_795)` → BPO2 (active since 1_767_747_671).
    let l1_cfg: Arc<L1ChainConfig> = Arc::new(L1Config::mainnet().into());

    // Real L1 header fields from Ethereum mainnet block #24618713.
    // Only the fields that affect L1BlockInfo computation are populated.
    let l1_header = Header {
        number: L1_BLOCK_NUMBER,
        timestamp: L1_BLOCK_TIMESTAMP,
        base_fee_per_gas: Some(L1_BASE_FEE),
        excess_blob_gas: Some(L1_EXCESS_BLOB_GAS),
        ..Default::default()
    };
    let l1_hash = l1_header.hash_slow();

    // SystemConfig for block #43127819.
    //
    // `scalar` is the post-Ecotone packed format (type byte = 0x01 in the high byte,
    // blob_base_fee_scalar in bytes 24-27, base_fee_scalar in bytes 28-31).  These values
    // were decoded from block #43127820's L1BlockInfo deposit tx.
    //
    // `da_footprint_gas_scalar` is None — simulating the production proof pipeline state
    // where the `DaFootprintGasScalarUpdate` L1 event had not yet been processed.  The
    // correct on-chain value is EXPECTED_DA_FOOTPRINT_GAS_SCALAR (139); the pipeline will
    // instead fall back to DEFAULT_DA_FOOTPRINT_GAS_SCALAR (400).
    let sys_config = SystemConfig {
        batcher_address: address!("5050f69a9786f081509234f1a7f4684b5e5b76c9"),
        gas_limit: 376_308_672,
        // Packed scalar: type=0x01, blob_base_fee_scalar=1055762, base_fee_scalar=2269.
        scalar: (U256::from(1u8) << 248)
            | (U256::from(1_055_762u32) << 32)
            | U256::from(2269u32),
        da_footprint_gas_scalar: None,
        ..Default::default()
    };

    let mut fetcher = TestSystemConfigL2Fetcher::default();
    fetcher.insert(L2_PARENT_NUMBER, sys_config);

    let mut provider = TestChainProvider::default();
    provider.insert_header(l1_hash, l1_header.clone());

    // L2 parent: block #43127819.  Both #43127819 and #43127820 share L1 origin #24618713
    // (same epoch), so derivation uses the same-epoch code path: no update_with_receipts,
    // and seq_num = l2_parent.seq_num + 1.
    let epoch = BlockNumHash { hash: l1_hash, number: L1_BLOCK_NUMBER };
    let l2_parent = L2BlockInfo {
        block_info: BlockInfo {
            number: L2_PARENT_NUMBER,
            timestamp: L2_PARENT_TIMESTAMP,
            hash: L2_PARENT_HASH,
            parent_hash: B256::ZERO,
        },
        l1_origin: epoch,
        seq_num: L2_PARENT_SEQ_NUM,
    };

    let next_l2_time = L2_PARENT_TIMESTAMP + rollup_cfg.block_time;
    assert!(
        next_l2_time >= L1_BLOCK_TIMESTAMP,
        "test setup: time invariant violated ({next_l2_time} < {L1_BLOCK_TIMESTAMP})",
    );

    let mut builder = StatefulAttributesBuilder::new(rollup_cfg, l1_cfg, fetcher, provider);
    let attrs = builder
        .prepare_payload_attributes(l2_parent, epoch)
        .await
        .expect("prepare_payload_attributes must succeed");

    let txs = attrs.transactions.expect("transactions must be present");
    assert!(!txs.is_empty(), "must have at least the L1 info deposit tx");

    let envelope =
        OpTxEnvelope::decode_2718(&mut txs[0].as_ref()).expect("valid 2718-encoded deposit");
    let OpTxEnvelope::Deposit(deposit) = envelope else {
        panic!("first tx must be a deposit");
    };
    let l1_info =
        L1BlockInfoTx::decode_calldata(&deposit.input).expect("calldata must be valid L1BlockInfo");
    let L1BlockInfoTx::Jovian(ref jovian) = l1_info else {
        panic!("expected Jovian L1BlockInfoTx, got {l1_info:?}");
    };

    // seq_num must be L2_PARENT_SEQ_NUM + 1 = 4 (same-epoch path: no new epoch).
    assert_eq!(
        l1_info.sequence_number(),
        EXPECTED_SEQ_NUM,
        "sequence_number must be {EXPECTED_SEQ_NUM} (parent {L2_PARENT_SEQ_NUM} + 1)",
    );

    // blob_base_fee must use BPO2 params: L1Config::mainnet() has BPO2 in blob_schedule,
    // and L1 timestamp 1_773_044_795 > BPO2 activation 1_767_747_671.
    assert_eq!(
        l1_info.blob_base_fee(),
        U256::from(EXPECTED_BLOB_BASE_FEE),
        "blob_base_fee must be {EXPECTED_BLOB_BASE_FEE} (BPO2 params, \
         excess_blob_gas={L1_EXCESS_BLOB_GAS})",
    );

    // ── This assertion FAILS with the current code ────────────────────────────────────────
    // `SystemConfig.da_footprint_gas_scalar` is `None` (DaFootprintGasScalarUpdate event
    // not processed by the proof pipeline for this block).  `L1BlockInfoTx::try_new` falls
    // back to `DEFAULT_DA_FOOTPRINT_GAS_SCALAR = 400`.  The correct on-chain value decoded
    // from block #43127820 is EXPECTED_DA_FOOTPRINT_GAS_SCALAR = 139.
    assert_eq!(
        jovian.da_footprint_gas_scalar,
        EXPECTED_DA_FOOTPRINT_GAS_SCALAR,
        "da_footprint_gas_scalar must be {} (on-chain value), \
         but the pipeline produced {} (default fallback when SystemConfig field is None)",
        EXPECTED_DA_FOOTPRINT_GAS_SCALAR,
        L1BlockInfoJovian::DEFAULT_DA_FOOTPRINT_GAS_SCALAR,
    );
}
