//! Pre-execution block-hook tests for `base-common-evm2`.
//!
//! Validates that [`BaseBlockExecutor::apply_pre_execution`] fires the EIP-4788 (beacon root)
//! and EIP-2935 (block hashes) system calls at the correct fork and with the correct data. Each
//! system contract is replaced with a stub that stores its calldata to slot 0, so the test
//! asserts the executor's gating and data-passing without depending on the real system-contract
//! bytecode.

use alloy_primitives::{B256, U256};
use base_common_evm2::{BaseBlockExecutionCtx, BaseBlockExecutor, BaseEvmTypes, BaseSpecId};
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

/// Builds an EVM at `upgrade` with the calldata-storing stub deployed at `system_contract`.
fn evm_with_stub(
    upgrade: BaseUpgrade,
    system_contract: alloy_primitives::Address,
) -> Evm<'static, BaseEvmTypes> {
    let spec = BaseSpecId::new(upgrade);
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &system_contract,
        evm2::AccountInfo {
            code: Some(Bytecode::new_legacy(STORE_CALLDATA_STUB.to_vec().into())),
            ..Default::default()
        },
    );
    Evm::new(
        spec,
        BlockEnv::<BaseEvmTypes>::default(),
        BaseEvmTypes::tx_registry(),
        db,
        Precompiles::base(spec.into()),
    )
}

fn slot0(evm: &mut Evm<'static, BaseEvmTypes>, addr: alloy_primitives::Address) -> U256 {
    evm.state_mut().storage_slot(&addr, U256::ZERO, false).unwrap().current()
}

#[test]
fn beacon_root_system_call_fires_at_ecotone() {
    let root = B256::repeat_byte(0xbe);
    let evm = evm_with_stub(BaseUpgrade::Ecotone, BEACON_ROOTS_ADDRESS);
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
    let evm = evm_with_stub(BaseUpgrade::Regolith, BEACON_ROOTS_ADDRESS);
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
    let evm = evm_with_stub(BaseUpgrade::Isthmus, HISTORY_STORAGE_ADDRESS);
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx { parent_hash, parent_beacon_block_root: None },
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
    let mut db = InMemoryDB::default();
    for addr in [HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS] {
        db.insert_account_info(
            &addr,
            evm2::AccountInfo {
                code: Some(Bytecode::new_legacy(STORE_CALLDATA_STUB.to_vec().into())),
                ..Default::default()
            },
        );
    }
    let spec = BaseSpecId::new(BaseUpgrade::Isthmus);
    let evm = Evm::new(
        spec,
        BlockEnv::<BaseEvmTypes>::default(),
        BaseEvmTypes::tx_registry(),
        db,
        Precompiles::base(spec.into()),
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
    // Ecotone maps to CANCUN, which is pre-Prague — the block-hashes call must not fire.
    let evm = evm_with_stub(BaseUpgrade::Ecotone, HISTORY_STORAGE_ADDRESS);
    let mut executor = BaseBlockExecutor::new(
        evm,
        BaseBlockExecutionCtx { parent_hash, parent_beacon_block_root: None },
    );
    executor.apply_pre_execution().expect("pre-execution succeeds");
    let (mut evm, _, _) = executor.finish();
    assert_eq!(slot0(&mut evm, HISTORY_STORAGE_ADDRESS), U256::ZERO);
}
