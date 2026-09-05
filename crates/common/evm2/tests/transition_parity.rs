//! Differential parity harness for the transition-block hooks.
//!
//! Runs each Base transition hook (Canyon create2-deployer, Zenith EIP-8130 system-account stub,
//! Cobalt `BaseTime` predeploy) through the `base-common-evm2` executor and, on an equivalent
//! database, through the revm-based `base-common-evm` reference function, asserting the two engines
//! install byte-identical state (the affected account code hashes and, for `BaseTime`, the linked
//! EIP-1967 implementation slot).

use alloy_primitives::{Address, B256, Bytes, U256, address, uint};
use base_common_consensus::Predeploys;
use base_common_evm::{
    BaseTime as RevmBaseTime, ensure_create2_deployer, ensure_eip8130_system_accounts,
};
use base_common_evm2::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseEvmTypes, BaseSpecId, BaseTime,
};
use base_common_genesis::{BaseUpgrade, RollupConfig, UpgradeConfig};
use evm2::{Evm, Precompiles, env::BlockEnv, evm::InMemoryDB};
use revm::{
    Database as _,
    database::InMemoryDB as RevmDb,
    state::{AccountInfo as RevmAccountInfo, Bytecode as RevmBytecode},
};

const CREATE2_DEPLOYER: Address = address!("0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2");
const NONCE_MANAGER: Address = address!("0x813000000000000000000000000000000000aa01");
const IMPLEMENTATION_SLOT: U256 =
    uint!(0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc_U256);
const ADMIN_SLOT: U256 =
    uint!(0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103_U256);
const ACTIVATION_TS: u64 = 1_000;

/// The evm2 fork schedule: `target` activates at `ACTIVATION_TS`, earlier execution upgrades at 0.
fn evm2_schedule(target: BaseUpgrade) -> UpgradeConfig {
    let mut config = UpgradeConfig::default();
    for upgrade in BaseUpgrade::EXECUTION_VARIANTS {
        if (upgrade as u8) < (target as u8) {
            config.set_activation_timestamp(upgrade, 0);
        }
    }
    config.set_activation_timestamp(target, ACTIVATION_TS);
    config
}

/// The revm fork schedule matching [`evm2_schedule`]: `target` at `ACTIVATION_TS`.
fn revm_schedule(target: BaseUpgrade) -> RollupConfig {
    let mut config = RollupConfig::default();
    config.set_upgrade_activation_timestamp(target, ACTIVATION_TS);
    config
}

/// Builds an evm2 executor at `upgrade` with the block timestamp at `ACTIVATION_TS` over `db`.
fn evm2_executor(upgrade: BaseUpgrade, db: InMemoryDB) -> BaseBlockExecutor<'static> {
    let spec = BaseSpecId::new(upgrade);
    let block =
        BlockEnv::<BaseEvmTypes> { timestamp: U256::from(ACTIVATION_TS), ..Default::default() };
    let evm =
        Evm::new(spec, block, BaseEvmTypes::tx_registry(), db, Precompiles::base(spec.into()));
    BaseBlockExecutor::new(evm, BaseBlockExecutionCtx::default())
}

fn evm2_code_hash(evm: &mut Evm<'static, BaseEvmTypes>, addr: Address) -> B256 {
    evm.state_mut()
        .account_info_untracked(&addr)
        .unwrap()
        .map(|i| i.code_hash)
        .unwrap_or(alloy_primitives::KECCAK256_EMPTY)
}

fn evm2_db_with_valid_base_time_proxy() -> InMemoryDB {
    let mut db = InMemoryDB::default();
    let proxy_code = Bytes::from_static(&[0x60, 0x00]);
    let proxy_bytecode = evm2::bytecode::Bytecode::new_raw(proxy_code);
    let proxy = evm2::AccountInfo::new(U256::ZERO, 0, proxy_bytecode.hash_slow(), proxy_bytecode);
    db.insert_account_info(&Predeploys::BASE_TIME, proxy);
    db.insert_account_storage(
        &Predeploys::BASE_TIME,
        &ADMIN_SLOT,
        &U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
    );
    db
}

fn revm_code_hash(db: &mut RevmDb, addr: Address) -> B256 {
    db.basic(addr).unwrap().map(|a| a.code_hash).unwrap_or(alloy_primitives::KECCAK256_EMPTY)
}

#[test]
fn canyon_create2_deployer_matches_revm() {
    // evm2.
    let mut executor = evm2_executor(BaseUpgrade::Canyon, InMemoryDB::default());
    executor.apply_transition_hooks(&evm2_schedule(BaseUpgrade::Canyon)).expect("hooks apply");
    let (mut evm, _, _) = executor.finish();
    let evm2_hash = evm2_code_hash(&mut evm, CREATE2_DEPLOYER);

    // revm reference.
    let mut db = RevmDb::default();
    ensure_create2_deployer(revm_schedule(BaseUpgrade::Canyon), ACTIVATION_TS, &mut db)
        .expect("reference applies");
    let revm_hash = revm_code_hash(&mut db, CREATE2_DEPLOYER);

    assert_ne!(evm2_hash, alloy_primitives::KECCAK256_EMPTY, "create2 deployer installed");
    assert_eq!(evm2_hash, revm_hash, "create2 deployer code hash diverged");
}

#[test]
fn zenith_system_account_stub_matches_revm() {
    // evm2 (Canyon active-at-0 so it does not re-fire; only Zenith does).
    let mut schedule = evm2_schedule(BaseUpgrade::Zenith);
    schedule.set_activation_timestamp(BaseUpgrade::Canyon, 0);
    let mut executor = evm2_executor(BaseUpgrade::Zenith, evm2_db_with_valid_base_time_proxy());
    executor.apply_transition_hooks(&schedule).expect("hooks apply");
    let (mut evm, _, _) = executor.finish();
    let evm2_hash = evm2_code_hash(&mut evm, NONCE_MANAGER);

    // revm reference.
    let mut db = RevmDb::default();
    ensure_eip8130_system_accounts(revm_schedule(BaseUpgrade::Zenith), ACTIVATION_TS, &mut db)
        .expect("reference applies");
    let revm_hash = revm_code_hash(&mut db, NONCE_MANAGER);

    assert_ne!(evm2_hash, alloy_primitives::KECCAK256_EMPTY, "system-account stub installed");
    assert_eq!(evm2_hash, revm_hash, "system-account stub code hash diverged");
}

#[test]
fn base_time_predeploy_matches_revm() {
    // evm2 side: seed a valid proxy, then run the hooks.
    let mut executor = evm2_executor(BaseUpgrade::Cobalt, evm2_db_with_valid_base_time_proxy());
    executor.apply_transition_hooks(&evm2_schedule(BaseUpgrade::Cobalt)).expect("hooks apply");
    let (mut evm, _, _) = executor.finish();
    let evm2_impl_hash = evm2_code_hash(&mut evm, BaseTime::IMPLEMENTATION_ADDRESS);
    let evm2_slot = evm
        .state_mut()
        .storage_slot_untracked(&Predeploys::BASE_TIME, &IMPLEMENTATION_SLOT)
        .unwrap();

    // revm side: seed the equivalent proxy, then run the reference.
    let mut db = RevmDb::default();
    let proxy_code = Bytes::from_static(&[0x60, 0x00]);
    let revm_proxy = RevmBytecode::new_raw(proxy_code);
    db.insert_account_info(
        Predeploys::BASE_TIME,
        RevmAccountInfo {
            code_hash: revm_proxy.hash_slow(),
            code: Some(revm_proxy),
            ..Default::default()
        },
    );
    db.insert_account_storage(
        Predeploys::BASE_TIME,
        ADMIN_SLOT,
        U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
    )
    .unwrap();
    RevmBaseTime::ensure_predeploy(revm_schedule(BaseUpgrade::Cobalt), ACTIVATION_TS, &mut db)
        .expect("reference applies");
    let revm_impl_hash = revm_code_hash(&mut db, RevmBaseTime::IMPLEMENTATION_ADDRESS);
    let revm_slot = db.storage(Predeploys::BASE_TIME, IMPLEMENTATION_SLOT).unwrap();

    assert_eq!(evm2_impl_hash, revm_impl_hash, "BaseTime implementation code hash diverged");
    assert_eq!(evm2_slot, revm_slot, "BaseTime EIP-1967 implementation slot diverged");
    assert_eq!(
        evm2_slot,
        U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice()),
        "implementation slot links to the implementation address",
    );
}
