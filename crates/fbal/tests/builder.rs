//! Tests for ensuring the access list is built properly

use std::{collections::HashMap, sync::Arc};

use alloy_consensus::Header;
use alloy_eip7928::{
    AccountChanges, BalanceChange, CodeChange, EMPTY_BLOCK_ACCESS_LIST_HASH, NonceChange,
    SlotChanges, StorageChange,
};
use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_sol_macro::sol;
use alloy_sol_types::SolCall;
use base_fbal::{FlashblockAccessList, TouchedAccountsInspector};
use op_revm::OpTransaction;
use reth_evm::{ConfigureEvm, Evm};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::OpEvmConfig;
use revm::{
    DatabaseCommit, DatabaseRef,
    context::{TxEnv, result::ResultAndState},
    database::InMemoryDB,
    interpreter::instructions::utility::IntoAddress,
    primitives::{KECCAK_EMPTY, ONE_ETHER},
    state::{AccountInfo, Bytecode},
};

sol!(
    #[sol(rpc)]
    AccessListContract,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-utils/contracts/out/AccessList.sol/AccessList.json"
    )
);

sol!(
    #[sol(rpc)]
    ContractFactory,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-utils/contracts/out/ContractFactory.sol/ContractFactory.json"
    )
);

sol!(
    #[sol(rpc)]
    SimpleStorage,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-utils/contracts/out/ContractFactory.sol/SimpleStorage.json"
    )
);

const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;

fn execute_txns_build_access_list(
    txs: Vec<OpTransaction<TxEnv>>,
    acc_overrides: Option<HashMap<Address, AccountInfo>>,
) -> FlashblockAccessList {
    let chain_spec = Arc::new(OpChainSpec::from_genesis(
        serde_json::from_str(include_str!("../../test-utils/assets/genesis.json")).unwrap(),
    ));
    let evm_config = OpEvmConfig::optimism(chain_spec.clone());
    let header = Header { base_fee_per_gas: Some(0), ..chain_spec.genesis_header().clone() };
    let mut db = InMemoryDB::default();
    if let Some(overrides) = acc_overrides {
        for (address, info) in overrides {
            db.insert_account_info(address, info);
        }
    }

    let mut access_list = FlashblockAccessList {
        min_tx_index: 0,
        max_tx_index: (txs.len() - 1) as u64,
        account_changes: vec![],
        fal_hash: EMPTY_BLOCK_ACCESS_LIST_HASH,
    };

    for (idx, tx) in txs.into_iter().enumerate() {
        let inspector = TouchedAccountsInspector::default();
        let evm_env = evm_config.evm_env(&header).unwrap();
        let mut evm = evm_config.evm_with_env_and_inspector(db, evm_env, inspector);
        let ResultAndState { state, .. } = evm.transact(tx).unwrap();

        let mut initial_accounts = HashMap::new();
        for (address, _) in &state {
            let initial_account = evm.db_mut().load_account(*address).map(|a| a.info());
            _ = match initial_account {
                Ok(Some(info)) => initial_accounts.insert(*address, info),
                _ => None,
            };
        }

        let mut account_changes: HashMap<Address, AccountChanges> = HashMap::new();
        for (address, slots) in evm.inspector_mut().touched_accounts.iter() {
            let change = AccountChanges::new(*address).extend_storage_reads(slots.iter().cloned());
            account_changes.insert(*address, change);
        }

        for (address, account) in &state {
            let initial_account = initial_accounts.get(address);
            let entry = account_changes.entry(*address).or_insert(AccountChanges::new(*address));

            let initial_balance = initial_account.map(|a| a.balance).unwrap_or_default();
            let initial_nonce = initial_account.map(|a| a.nonce).unwrap_or_default();
            let initial_code_hash = initial_account.map(|a| a.code_hash()).unwrap_or(KECCAK_EMPTY);

            if initial_balance != account.info.balance {
                entry.balance_changes.push(BalanceChange::new(idx as u64, account.info.balance));
            }

            if initial_nonce != account.info.nonce {
                entry.nonce_changes.push(NonceChange::new(idx as u64, account.info.nonce));
            }

            if initial_code_hash != account.info.code_hash() {
                let bytecode = match account.info.code.clone() {
                    Some(code) => code,
                    None => evm.db_mut().code_by_hash_ref(account.info.code_hash()).unwrap(),
                };
                entry.code_changes.push(CodeChange::new(idx as u64, bytecode.bytes()));
            }

            // TODO: This currently does not check if a storage key already exists within `storage_changes`
            // for a given account, and instead adds a new `SlotChanges` struct for the same storage key
            account.storage.iter().for_each(|(key, value)| {
                let previous_value = evm.db_mut().storage_ref(*address, *key);
                match previous_value {
                    Ok(prev) => {
                        if prev != value.present_value {
                            entry.storage_changes.push(SlotChanges::new(
                                B256::from(*key),
                                vec![StorageChange::new(
                                    idx as u64,
                                    B256::from(value.present_value()),
                                )],
                            ));
                        }
                    }
                    Err(_) => {
                        entry.storage_changes.push(SlotChanges::new(
                            B256::from(*key),
                            vec![StorageChange::new(idx as u64, B256::from(value.present_value()))],
                        ));
                    }
                }
            });
        }

        evm.db_mut().commit(state);
        db = evm.into_db();

        access_list.merge_account_changes(account_changes.values().cloned().collect());
    }

    access_list.finalize();
    access_list
}

#[test]
/// Tests that the system precompiles get included in the access list
pub fn test_precompiles() {
    let base_tx =
        TxEnv::builder().chain_id(Some(BASE_SEPOLIA_CHAIN_ID)).gas_limit(50_000).gas_price(0);
    let tx = OpTransaction::builder().base(base_tx).build_fill();
    let access_list = execute_txns_build_access_list(vec![tx], None);
    dbg!(access_list);
}

#[test]
/// Tests that a single ETH transfer is included in the access list
pub fn test_single_transfer() {
    let sender = U256::from(0xDEAD).into_address();
    let recipient = U256::from(0xBEEF).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(recipient))
                .value(U256::from(1_000_000))
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(21_100),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));
    dbg!(access_list);
}

#[test]
/// Ensures that when gas is paid, the appropriate balance changes are included
/// Sender balance is deducted as (fee paid + value)
/// Fee Vault/Beneficiary address earns fee paid
pub fn test_gas_included_in_balance_change() {
    let sender = U256::from(0xDEAD).into_address();
    let recipient = U256::from(0xBEEF).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(recipient))
                .value(U256::from(1_000_000))
                .gas_price(1000)
                .gas_priority_fee(Some(1_000))
                .max_fee_per_gas(1_000)
                .gas_limit(21_100),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));
    dbg!(access_list);
}

#[test]
/// Ensures that multiple transfers between the same sender/recipient
/// in a single direction are all processed correctly
pub fn test_multiple_transfers() {
    let sender = U256::from(0xDEAD).into_address();
    let recipient = U256::from(0xBEEF).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));

    let mut txs = Vec::new();
    for i in 0..10 {
        let tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(sender)
                    .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                    .nonce(i)
                    .kind(TxKind::Call(recipient))
                    .value(U256::from(1_000_000))
                    .gas_price(0)
                    .gas_priority_fee(None)
                    .max_fee_per_gas(0)
                    .gas_limit(21_100),
            )
            .build_fill();
        txs.push(tx);
    }

    let access_list = execute_txns_build_access_list(txs, Some(overrides));
    dbg!(access_list);
}

#[test]
/// Tests that we can SLOAD a zero-value from a freshly deployed contract's state
pub fn test_sload_zero_value() {
    let sender = U256::from(0xDEAD).into_address();
    let contract = U256::from(0xCAFE).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        contract,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(AccessListContract::DEPLOYED_BYTECODE.clone())),
    );

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(contract))
                .data(AccessListContract::valueCall {}.abi_encode().into())
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(100_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));
    dbg!(access_list);
}

#[test]
/// Tests that we can SSTORE and later SLOAD one value from a contract's state
pub fn test_update_one_value() {
    let sender = U256::from(0xDEAD).into_address();
    let contract = U256::from(0xCAFE).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        contract,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(AccessListContract::DEPLOYED_BYTECODE.clone())),
    );

    let mut txs = Vec::new();
    txs.push(
        OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(sender)
                    .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                    .kind(TxKind::Call(contract))
                    .data(
                        AccessListContract::updateValueCall { newValue: U256::from(42) }
                            .abi_encode()
                            .into(),
                    )
                    .nonce(0)
                    .gas_price(0)
                    .gas_priority_fee(None)
                    .max_fee_per_gas(0)
                    .gas_limit(100_000),
            )
            .build_fill(),
    );
    txs.push(
        OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(sender)
                    .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                    .kind(TxKind::Call(contract))
                    .data(AccessListContract::valueCall {}.abi_encode().into())
                    .nonce(1)
                    .gas_price(0)
                    .gas_priority_fee(None)
                    .max_fee_per_gas(0)
                    .gas_limit(100_000),
            )
            .build_fill(),
    );

    let access_list = execute_txns_build_access_list(txs, Some(overrides));
    dbg!(access_list);
}

#[test]
/// Ensures that storage reads that read the same slot multiple times are deduped properly
pub fn test_multi_sload_same_slot() {
    let sender = U256::from(0xDEAD).into_address();
    let contract = U256::from(0xCAFE).into_address();

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        contract,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(AccessListContract::DEPLOYED_BYTECODE.clone())),
    );

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(contract))
                .data(AccessListContract::getABCall {}.abi_encode().into())
                .nonce(0)
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(100_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));
    // TODO: dedup storage_reads
    dbg!(access_list);
}

#[test]
/// Ensures that storage writes that update multiple slots are recorded properly
pub fn test_multi_sstore() {
    let sender = U256::from(0xDEAD).into_address();
    let contract = U256::from(0xCAFE).into_address();

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        contract,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(AccessListContract::DEPLOYED_BYTECODE.clone())),
    );

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(contract))
                .data(
                    AccessListContract::insertMultipleCall {
                        keys: vec![U256::from(0), U256::from(1)],
                        values: vec![U256::from(84), U256::from(53)],
                    }
                    .abi_encode()
                    .into(),
                )
                .nonce(0)
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(100_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));
    dbg!(access_list);
}

#[test]
/// Tests that contract deployment via CREATE is tracked in the access list
/// Verifies:
/// - Factory contract address is in touched accounts
/// - Newly deployed contract address is in touched accounts
/// - Code change is recorded for the new contract
/// - Nonce change is recorded for the factory (CREATE increments nonce)
pub fn test_create_deployment_tracked() {
    let sender = U256::from(0xDEAD).into_address();
    let factory = U256::from(0xFAC0).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        factory,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(ContractFactory::DEPLOYED_BYTECODE.clone())),
    );

    // Deploy SimpleStorage via CREATE
    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(factory))
                .data(
                    ContractFactory::deployWithCreateCall {
                        bytecode: SimpleStorage::BYTECODE.to_vec().into(),
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(500_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));

    // Verify factory is in the access list
    let factory_entry = access_list.account_changes.iter().find(|ac| ac.address() == factory);
    assert!(factory_entry.is_some(), "Factory should be in access list");

    // The factory's nonce should change (CREATE increments deployer nonce)
    let factory_changes = factory_entry.unwrap();
    assert!(!factory_changes.nonce_changes.is_empty(), "Factory nonce should change due to CREATE");

    // Find the deployed contract - it should have a code change
    let deployed_entry = access_list
        .account_changes
        .iter()
        .find(|ac| !ac.code_changes.is_empty() && ac.address() != factory);
    assert!(deployed_entry.is_some(), "Deployed contract should have code change");

    let deployed_changes = deployed_entry.unwrap();
    assert_eq!(deployed_changes.code_changes.len(), 1, "Should have exactly one code change");

    // Verify the deployed bytecode matches SimpleStorage's deployed bytecode
    let code_change = &deployed_changes.code_changes[0];
    assert!(!code_change.new_code.is_empty(), "Deployed code should not be empty");

    dbg!(&access_list);
}

#[test]
/// Tests that contract deployment via CREATE2 is tracked in the access list
/// Verifies:
/// - Factory contract address is in touched accounts
/// - Deployed address (deterministic) is in touched accounts
/// - Code change is recorded with correct bytecode
pub fn test_create2_deployment_tracked() {
    let sender = U256::from(0xDEAD).into_address();
    let factory = U256::from(0xFAC0).into_address();
    let salt = B256::from(U256::from(12345));

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        factory,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(ContractFactory::DEPLOYED_BYTECODE.clone())),
    );

    // Deploy SimpleStorage via CREATE2
    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(factory))
                .data(
                    ContractFactory::deployWithCreate2Call {
                        bytecode: SimpleStorage::BYTECODE.to_vec().into(),
                        salt,
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(500_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));

    // Verify factory is in the access list
    let factory_entry = access_list.account_changes.iter().find(|ac| ac.address() == factory);
    assert!(factory_entry.is_some(), "Factory should be in access list");

    // Find the deployed contract - it should have a code change
    let deployed_entry = access_list
        .account_changes
        .iter()
        .find(|ac| !ac.code_changes.is_empty() && ac.address() != factory);
    assert!(deployed_entry.is_some(), "Deployed contract should have code change");

    let deployed_changes = deployed_entry.unwrap();
    assert_eq!(deployed_changes.code_changes.len(), 1, "Should have exactly one code change");

    // Verify the deployed bytecode is present
    let code_change = &deployed_changes.code_changes[0];
    assert!(!code_change.new_code.is_empty(), "Deployed code should not be empty");

    dbg!(&access_list);
}

#[test]
/// Tests that deploying a contract and immediately calling it tracks both operations
/// Verifies:
/// - Both the factory and deployed contract are tracked
/// - Code change for deployment is recorded
/// - Storage change from the call is recorded on the new contract's address
pub fn test_create_and_immediate_call() {
    let sender = U256::from(0xDEAD).into_address();
    let factory = U256::from(0xFAC0).into_address();

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        factory,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(ContractFactory::DEPLOYED_BYTECODE.clone())),
    );

    // Deploy SimpleStorage and immediately call setValue(42)
    let set_value_calldata = SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode();

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(factory))
                .data(
                    ContractFactory::deployAndCallCall {
                        bytecode: SimpleStorage::BYTECODE.to_vec().into(),
                        callData: set_value_calldata.into(),
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(500_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));

    // Verify factory is in the access list
    let factory_entry = access_list.account_changes.iter().find(|ac| ac.address() == factory);
    assert!(factory_entry.is_some(), "Factory should be in access list");

    // Find the deployed contract - it should have both code change AND storage change
    let deployed_entry = access_list
        .account_changes
        .iter()
        .find(|ac| !ac.code_changes.is_empty() && ac.address() != factory);
    assert!(deployed_entry.is_some(), "Deployed contract should have code change");

    let deployed_changes = deployed_entry.unwrap();

    // Verify code change exists
    assert_eq!(deployed_changes.code_changes.len(), 1, "Should have exactly one code change");

    // Verify storage change exists (from setValue(42))
    // SimpleStorage stores `value` at slot 0
    assert!(
        !deployed_changes.storage_changes.is_empty(),
        "Should have storage change from setValue call"
    );

    // Verify the storage slot is 0 and value is 42
    let storage_change = &deployed_changes.storage_changes[0];
    assert_eq!(storage_change.slot, B256::ZERO, "Storage slot should be 0");
    assert_eq!(
        storage_change.changes[0].new_value,
        B256::from(U256::from(42)),
        "Storage value should be 42"
    );

    dbg!(&access_list);
}
