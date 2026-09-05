//! Transition-block hook tests for `base-common-evm2`.
//!
//! Validates the Canyon create2-deployer, Cobalt `BaseTime`, and Zenith EIP-8130 system-account
//! irregular state transitions applied by [`BaseBlockExecutor::apply_transition_hooks`]: their
//! fork gating, the state they install, and their idempotency. Each hook's parity with the revm
//! reference (`base-common-evm`'s `canyon`/`zenith`/`base_time` modules) is in the shape of the
//! installed code/storage, asserted here against the same addresses and constants.

use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, address, b256, uint};
use base_common_consensus::Predeploys;
use base_common_evm2::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseEvmTypes, BaseSpecId, BaseTime,
};
use base_common_genesis::{BaseUpgrade, UpgradeConfig};
use evm2::{Evm, Precompiles, bytecode::Bytecode, env::BlockEnv, evm::InMemoryDB};

const CREATE2_DEPLOYER: Address = address!("0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2");
const NONCE_MANAGER: Address = address!("0x813000000000000000000000000000000000aa01");
const IMPLEMENTATION_SLOT: U256 =
    uint!(0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc_U256);
const ADMIN_SLOT: U256 =
    uint!(0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103_U256);
/// The codehash of the create2 deployer contract force-deployed at Canyon.
const CREATE2_DEPLOYER_CODEHASH: B256 =
    b256!("0xb0550b5b431e30d38000efb7107aaa0ade03d48a7198a140edda9d27134468b2");

/// Builds an `UpgradeConfig` with the given upgrade activated at `timestamp` (and every earlier
/// execution upgrade activated at 0, so the ladder is consistent).
fn schedule(upgrade: BaseUpgrade, timestamp: u64) -> UpgradeConfig {
    let mut config = UpgradeConfig::default();
    for u in BaseUpgrade::EXECUTION_VARIANTS {
        if (u as u8) < (upgrade as u8) {
            config.set_activation_timestamp(u, 0);
        }
    }
    config.set_activation_timestamp(upgrade, timestamp);
    config
}

/// Builds an executor at `upgrade` with block timestamp `timestamp` over `db`.
fn executor(upgrade: BaseUpgrade, timestamp: u64, db: InMemoryDB) -> BaseBlockExecutor<'static> {
    let spec = BaseSpecId::new(upgrade);
    let block = BlockEnv::<BaseEvmTypes> { timestamp: U256::from(timestamp), ..Default::default() };
    let evm =
        Evm::new(spec, block, BaseEvmTypes::tx_registry(), db, Precompiles::base(spec.into()));
    BaseBlockExecutor::new(evm, BaseBlockExecutionCtx::default())
}

fn code_hash(evm: &mut Evm<'static, BaseEvmTypes>, addr: Address) -> B256 {
    evm.state_mut()
        .account_info_untracked(&addr)
        .unwrap()
        .map(|i| i.code_hash)
        .unwrap_or(KECCAK256_EMPTY)
}

fn slot(evm: &mut Evm<'static, BaseEvmTypes>, addr: Address, key: U256) -> U256 {
    evm.state_mut().storage_slot_untracked(&addr, &key).unwrap()
}

#[test]
fn canyon_force_deploys_create2_on_activation_block() {
    // Canyon activates at ts=1000; the block at ts=1000 is the activation block.
    let mut executor = executor(BaseUpgrade::Canyon, 1000, InMemoryDB::default());
    executor.apply_transition_hooks(&schedule(BaseUpgrade::Canyon, 1000)).expect("hooks apply");
    let (mut evm, _, block_state) = executor.finish();

    assert_eq!(
        code_hash(&mut evm, CREATE2_DEPLOYER),
        CREATE2_DEPLOYER_CODEHASH,
        "create2 deployer code installed",
    );
    // The transition is recorded in the block-state delta.
    assert!(!block_state.is_empty(), "the force-deploy is in the block-state delta");
}

#[test]
fn canyon_is_skipped_after_the_activation_block() {
    // Canyon activated at ts=1000; a later block (ts=2000) is not the activation block.
    let mut executor = executor(BaseUpgrade::Canyon, 2000, InMemoryDB::default());
    executor.apply_transition_hooks(&schedule(BaseUpgrade::Canyon, 1000)).expect("hooks apply");
    let (mut evm, _, _) = executor.finish();

    assert_eq!(
        code_hash(&mut evm, CREATE2_DEPLOYER),
        KECCAK256_EMPTY,
        "no force-deploy after activation block",
    );
}

#[test]
fn zenith_plants_stub_on_codeless_nonce_manager() {
    let mut executor = executor(BaseUpgrade::Zenith, 1000, db_with_valid_proxy());
    executor.apply_transition_hooks(&schedule(BaseUpgrade::Zenith, 1000)).expect("hooks apply");
    let (mut evm, _, _) = executor.finish();

    let stub_hash = Bytecode::new_legacy(Bytes::from_static(&[0xEF])).hash_slow();
    assert_eq!(code_hash(&mut evm, NONCE_MANAGER), stub_hash, "the 0xEF stub is planted");
    assert_ne!(stub_hash, KECCAK256_EMPTY);
}

#[test]
fn zenith_does_not_overwrite_existing_code() {
    let real = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
    let real_hash = real.hash_slow();
    let mut db = db_with_valid_proxy();
    db.insert_account_info(&NONCE_MANAGER, evm2::AccountInfo::new(U256::ZERO, 0, real_hash, real));
    let mut executor = executor(BaseUpgrade::Zenith, 1000, db);
    executor.apply_transition_hooks(&schedule(BaseUpgrade::Zenith, 1000)).expect("hooks apply");
    let (mut evm, _, _) = executor.finish();

    assert_eq!(code_hash(&mut evm, NONCE_MANAGER), real_hash, "existing deployment preserved");
}

#[test]
fn zenith_is_skipped_before_activation() {
    let mut executor = executor(BaseUpgrade::Isthmus, 1000, db_with_valid_proxy());
    // Zenith is scheduled far in the future; at ts=1000 it is inactive.
    executor.apply_transition_hooks(&schedule(BaseUpgrade::Zenith, 10_000)).expect("hooks apply");
    let (mut evm, _, _) = executor.finish();

    assert_eq!(code_hash(&mut evm, NONCE_MANAGER), KECCAK256_EMPTY, "no stub before Zenith");
}

/// Seeds a valid `BaseTime` proxy (with code and the canonical admin) so the transition can link
/// its implementation slot.
fn db_with_valid_proxy() -> InMemoryDB {
    let mut db = InMemoryDB::default();
    let proxy_code = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
    db.insert_account_info(
        &Predeploys::BASE_TIME,
        evm2::AccountInfo::new(U256::ZERO, 0, proxy_code.hash_slow(), proxy_code),
    );
    db.insert_account_storage(
        &Predeploys::BASE_TIME,
        &ADMIN_SLOT,
        &U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
    );
    db
}

#[test]
fn base_time_installs_and_links_implementation_on_cobalt() {
    let mut executor = executor(BaseUpgrade::Cobalt, 1000, db_with_valid_proxy());
    executor.apply_transition_hooks(&schedule(BaseUpgrade::Cobalt, 1000)).expect("hooks apply");
    let (mut evm, _, _) = executor.finish();

    // The implementation runtime is installed at its code-namespace address.
    assert_eq!(
        code_hash(&mut evm, BaseTime::IMPLEMENTATION_ADDRESS),
        BaseTime::IMPLEMENTATION_CODE_HASH,
    );
    // The proxy's EIP-1967 implementation slot points at the implementation address.
    assert_eq!(
        slot(&mut evm, Predeploys::BASE_TIME, IMPLEMENTATION_SLOT),
        U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice()),
    );
}

#[test]
fn base_time_errors_on_missing_proxy() {
    let mut executor = executor(BaseUpgrade::Cobalt, 1000, InMemoryDB::default());
    let err = executor
        .apply_transition_hooks(&schedule(BaseUpgrade::Cobalt, 1000))
        .expect_err("missing proxy is rejected");
    assert!(format!("{err}").contains("reserved proxy account"), "got: {err}");
}

#[test]
fn base_time_errors_on_codeless_proxy() {
    // The reserved proxy account exists (non-zero balance) but has no code: activation cannot
    // link an implementation into a bare account, so it must be rejected.
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &Predeploys::BASE_TIME,
        evm2::AccountInfo { balance: U256::from(1), ..Default::default() },
    );
    let mut executor = executor(BaseUpgrade::Cobalt, 1000, db);
    let err = executor
        .apply_transition_hooks(&schedule(BaseUpgrade::Cobalt, 1000))
        .expect_err("codeless proxy is rejected");
    assert!(format!("{err}").contains("existing proxy code"), "got: {err}");
}

#[test]
fn base_time_errors_on_unexpected_proxy_admin() {
    // A proxy with code but the wrong EIP-1967 admin is not the canonical predeploy: activation
    // must reject it rather than link an implementation behind an unexpected admin.
    let mut db = db_with_valid_proxy();
    db.insert_account_storage(&Predeploys::BASE_TIME, &ADMIN_SLOT, &U256::from(0xdead_u64));
    let mut executor = executor(BaseUpgrade::Cobalt, 1000, db);
    let err = executor
        .apply_transition_hooks(&schedule(BaseUpgrade::Cobalt, 1000))
        .expect_err("unexpected proxy admin is rejected");
    assert!(format!("{err}").contains("canonical proxy admin"), "got: {err}");
}

#[test]
fn base_time_is_idempotent_when_already_linked() {
    let mut db = db_with_valid_proxy();
    // Pre-link the implementation slot: the transition must be a no-op and preserve it.
    let existing = address!("0x1111111111111111111111111111111111111111");
    db.insert_account_storage(
        &Predeploys::BASE_TIME,
        &IMPLEMENTATION_SLOT,
        &U256::from_be_slice(existing.as_slice()),
    );
    let mut executor = executor(BaseUpgrade::Cobalt, 1000, db);
    executor.apply_transition_hooks(&schedule(BaseUpgrade::Cobalt, 1000)).expect("hooks apply");
    let (mut evm, _, _) = executor.finish();

    assert_eq!(
        slot(&mut evm, Predeploys::BASE_TIME, IMPLEMENTATION_SLOT),
        U256::from_be_slice(existing.as_slice()),
        "existing linkage preserved",
    );
    assert_eq!(
        code_hash(&mut evm, BaseTime::IMPLEMENTATION_ADDRESS),
        KECCAK256_EMPTY,
        "no implementation install",
    );
}
