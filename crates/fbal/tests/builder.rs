//! Tests for ensuring the access list is built properly

use std::{collections::HashMap, sync::Arc};

use alloy_consensus::Header;
use alloy_eip7928::{AccountChanges, BalanceChange, CodeChange, NonceChange};
use alloy_primitives::{Address, B256, TxKind, U256};
use base_fbal::{FlashblockAccessList, TouchedAccountsInspector};
use op_revm::{DefaultOp, OpBuilder, OpContext, OpSpecId, OpTransaction};
use reth_evm::{ConfigureEvm, Evm};
use reth_optimism_chainspec::{BASE_MAINNET, OpChainSpec};
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use revm::{
    Context, DatabaseCommit, ExecuteCommitEvm, ExecuteEvm, InspectEvm, MainBuilder, MainContext,
    context::{CfgEnv, ContextTr, TxEnv, result::ResultAndState},
    database::InMemoryDB,
    inspector::JournalExt,
    interpreter::instructions::utility::IntoAddress,
    state::AccountInfo,
};

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
        fal_hash: B256::ZERO,
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
            let initially_no_code =
                initial_account.map(|a| a.is_empty_code_hash()).unwrap_or_default();

            if initial_balance != account.info.balance {
                entry.balance_changes.push(BalanceChange::new(idx as u64, account.info.balance));
            }

            if initial_nonce != account.info.nonce {
                entry.nonce_changes.push(NonceChange::new(idx as u64, account.info.nonce));
            }

            if initially_no_code && !account.info.is_empty_code_hash() {
                entry
                    .code_changes
                    .push(CodeChange::new(idx as u64, account.info.code.as_ref().unwrap().bytes()));
            }
        }

        evm.db_mut().commit(state);
        db = evm.into_db();

        access_list.merge_account_changes(account_changes.values().cloned().collect());
        // access_list.account_changes.extend(account_changes.values().cloned());
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
    overrides.insert(sender, AccountInfo::from_balance(U256::from(1_000_000_000u32)));

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
    overrides.insert(sender, AccountInfo::from_balance(U256::from(1_000_000_000u32)));

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
    overrides.insert(sender, AccountInfo::from_balance(U256::from(1_000_000_000u32)));

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
