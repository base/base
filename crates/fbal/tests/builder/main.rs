//! Tests for ensuring the access list is built properly

use std::{collections::HashMap, sync::Arc};

use alloy_consensus::Header;
use alloy_eip7928::{
    AccountChanges, BalanceChange, CodeChange, EMPTY_BLOCK_ACCESS_LIST_HASH, NonceChange,
    SlotChanges, StorageChange,
};
use alloy_primitives::{Address, B256};
use alloy_sol_macro::sol;
use base_fbal::{FlashblockAccessList, TouchedAccountsInspector};
use op_revm::OpTransaction;
use reth_evm::{ConfigureEvm, Evm};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::OpEvmConfig;
use revm::{
    DatabaseCommit, DatabaseRef,
    context::{TxEnv, result::ResultAndState},
    database::InMemoryDB,
    primitives::KECCAK_EMPTY,
    state::AccountInfo,
};

mod deployment;
mod storage;
mod transfers;

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
        serde_json::from_str(include_str!("../../../test-utils/assets/genesis.json")).unwrap(),
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
