//! Pre-execution block-hook tests for `base-common-evm2`.
//!
//! Validates that [`BaseBlockExecutor::apply_pre_execution`] fires the EIP-4788 (beacon root)
//! and EIP-2935 (block hashes) system calls at the correct fork and with the correct data, and
//! that it enforces the reference's block-boundary validation (genesis no-ops, post-Cancun
//! beacon-root requirement). Each system contract is replaced with a stub that stores its
//! calldata to slot 0, so the test asserts the executor's gating and data-passing without
//! depending on the real system-contract bytecode.

use alloy_primitives::{Address, B256, U256};
use base_common_evm2::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseEvmTypes, BaseSpecId, PreExecutionError,
};
use base_common_genesis::BaseUpgrade;
use evm2::{
    Evm, Precompiles,
    bytecode::Bytecode,
    env::BlockEnv,
    evm::{BEACON_ROOTS_ADDRESS, HISTORY_STORAGE_ADDRESS, InMemoryDB},
};

/// Runtime code that stores `calldata[0:32]` to storage slot 0:
/// `PUSH1 0x00, CALLDATALOAD, PUSH1 0x00, SSTORE, STOP`.
const STORE_CALLDATA_STUB: [u8; 7] = [0x60, 0x00, 0x35, 0x60, 0x00, 0x55, 0x00];

/// A non-genesis block number. The pre-execution system calls only run for non-genesis blocks,
/// so tests that expect a call to fire must execute against a block past genesis.
const NORMAL_BLOCK: u64 = 1;

/// Builds an EVM at `upgrade` and block `number` with the calldata-storing stub deployed at each
/// address in `system_contracts`.
fn evm_with_stubs(
    upgrade: BaseUpgrade,
    number: u64,
    system_contracts: &[Address],
) -> Evm<'static, BaseEvmTypes> {
    let spec = BaseSpecId::new(upgrade);
    let mut db = InMemoryDB::default();
    for addr in system_contracts {
        db.insert_account_info(
            addr,
            evm2::AccountInfo {
                code: Some(Bytecode::new_legacy(STORE_CALLDATA_STUB.to_vec().into())),
                ..Default::default()
            },
        );
    }
    Evm::new(
        spec,
        BlockEnv::<BaseEvmTypes> { number: U256::from(number), ..Default::default() },
        BaseEvmTypes::tx_registry(),
        db,
        Precompiles::base(spec.into()),
    )
}

fn slot0(evm: &mut Evm<'static, BaseEvmTypes>, addr: Address) -> U256 {
    evm.state_mut().storage_slot(&addr, U256::ZERO, false).unwrap().current()
}

#[test]
fn beacon_root_system_call_fires_at_ecotone() {
    let root = B256::repeat_byte(0xbe);
    let evm = evm_with_stubs(BaseUpgrade::Ecotone, NORMAL_BLOCK, &[BEACON_ROOTS_ADDRESS]);
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx { parent_beacon_block_root: Some(root), ..Default::default() },
    );
    executor.apply_pre_execution().expect("pre-execution succeeds");
    let (mut evm, _, _) = executor.finish();
    // The beacon-roots system call ran with the parent beacon root as calldata.
    assert_eq!(slot0(&mut evm, BEACON_ROOTS_ADDRESS), U256::from_be_slice(root.as_slice()));
}

#[test]
fn beacon_root_system_call_is_skipped_before_cancun() {
    let root = B256::repeat_byte(0xbe);
    // Regolith maps to MERGE, which is pre-Cancun — the beacon-roots call must not fire.
    let evm = evm_with_stubs(BaseUpgrade::Regolith, NORMAL_BLOCK, &[BEACON_ROOTS_ADDRESS]);
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx { parent_beacon_block_root: Some(root), ..Default::default() },
    );
    executor.apply_pre_execution().expect("pre-execution succeeds");
    let (mut evm, _, _) = executor.finish();
    assert_eq!(slot0(&mut evm, BEACON_ROOTS_ADDRESS), U256::ZERO);
}

#[test]
fn block_hashes_system_call_fires_at_isthmus() {
    let parent_hash = B256::repeat_byte(0x29);
    // Isthmus maps to PRAGUE (≥ CANCUN), so the block must also carry a beacon root. Only the
    // block-hashes stub is deployed here, so the beacon-roots call no-ops and this test isolates
    // the block-hashes call.
    let evm = evm_with_stubs(BaseUpgrade::Isthmus, NORMAL_BLOCK, &[HISTORY_STORAGE_ADDRESS]);
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx {
            parent_hash,
            parent_beacon_block_root: Some(B256::repeat_byte(0xbe)),
        },
    );
    executor.apply_pre_execution().expect("pre-execution succeeds");
    let (mut evm, _, _) = executor.finish();
    // The block-hashes system call (Prague onwards) ran with the parent hash as calldata.
    assert_eq!(
        slot0(&mut evm, HISTORY_STORAGE_ADDRESS),
        U256::from_be_slice(parent_hash.as_slice())
    );
}

#[test]
fn both_system_calls_fire_at_isthmus() {
    // Isthmus maps to PRAGUE (≥ CANCUN), so both EIP-2935 and EIP-4788 should fire and both
    // state changes should commit independently.
    let parent_hash = B256::repeat_byte(0x29);
    let beacon_root = B256::repeat_byte(0xbe);
    let evm = evm_with_stubs(
        BaseUpgrade::Isthmus,
        NORMAL_BLOCK,
        &[HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS],
    );
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx { parent_hash, parent_beacon_block_root: Some(beacon_root) },
    );
    executor.apply_pre_execution().expect("pre-execution succeeds");
    let (mut evm, _, _) = executor.finish();
    assert_eq!(
        slot0(&mut evm, HISTORY_STORAGE_ADDRESS),
        U256::from_be_slice(parent_hash.as_slice())
    );
    assert_eq!(slot0(&mut evm, BEACON_ROOTS_ADDRESS), U256::from_be_slice(beacon_root.as_slice()));
}

#[test]
fn block_hashes_system_call_is_skipped_before_prague() {
    let parent_hash = B256::repeat_byte(0x29);
    // Ecotone maps to CANCUN, which is pre-Prague — the block-hashes call must not fire. Cancun
    // is active, so a beacon root is still required; the beacon stub is absent so that call
    // no-ops.
    let evm = evm_with_stubs(BaseUpgrade::Ecotone, NORMAL_BLOCK, &[HISTORY_STORAGE_ADDRESS]);
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx {
            parent_hash,
            parent_beacon_block_root: Some(B256::repeat_byte(0xbe)),
        },
    );
    executor.apply_pre_execution().expect("pre-execution succeeds");
    let (mut evm, _, _) = executor.finish();
    assert_eq!(slot0(&mut evm, HISTORY_STORAGE_ADDRESS), U256::ZERO);
}

#[test]
fn missing_beacon_root_is_rejected_post_cancun() {
    // Post-Cancun the reference rejects a block that carries no parent beacon block root; the
    // executor must surface that as an error rather than silently skipping the 4788 update.
    let evm = evm_with_stubs(BaseUpgrade::Ecotone, NORMAL_BLOCK, &[BEACON_ROOTS_ADDRESS]);
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx { parent_beacon_block_root: None, ..Default::default() },
    );
    let err = executor.apply_pre_execution().expect_err("missing beacon root must be rejected");
    assert_eq!(
        err.external_ref::<PreExecutionError>(),
        Some(&PreExecutionError::MissingParentBeaconBlockRoot)
    );
    // The beacon-roots call never ran, so its slot is untouched.
    let (mut evm, _, _) = executor.finish();
    assert_eq!(slot0(&mut evm, BEACON_ROOTS_ADDRESS), U256::ZERO);
}

#[test]
fn genesis_block_skips_system_calls() {
    // At genesis (block 0) neither EIP-2935 nor EIP-4788 runs, even at a fork where both are
    // active. The beacon root must be zero, matching the reference.
    let parent_hash = B256::repeat_byte(0x29);
    let evm =
        evm_with_stubs(BaseUpgrade::Isthmus, 0, &[HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS]);
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx { parent_hash, parent_beacon_block_root: Some(B256::ZERO) },
    );
    executor.apply_pre_execution().expect("genesis pre-execution succeeds");
    let (mut evm, _, _) = executor.finish();
    assert_eq!(slot0(&mut evm, HISTORY_STORAGE_ADDRESS), U256::ZERO);
    assert_eq!(slot0(&mut evm, BEACON_ROOTS_ADDRESS), U256::ZERO);
}

#[test]
fn genesis_nonzero_beacon_root_is_rejected() {
    // A Cancun genesis block must carry a zero beacon root; a non-zero root is invalid.
    let root = B256::repeat_byte(0xbe);
    let evm = evm_with_stubs(BaseUpgrade::Ecotone, 0, &[BEACON_ROOTS_ADDRESS]);
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx { parent_beacon_block_root: Some(root), ..Default::default() },
    );
    let err =
        executor.apply_pre_execution().expect_err("non-zero genesis beacon root must be rejected");
    assert_eq!(
        err.external_ref::<PreExecutionError>(),
        Some(&PreExecutionError::CancunGenesisParentBeaconBlockRootNotZero {
            parent_beacon_block_root: root,
        })
    );
}
