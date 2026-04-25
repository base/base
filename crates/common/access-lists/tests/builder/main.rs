//! Tests for ensuring the access list is built properly

use std::collections::HashMap;

use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_sol_types::SolCall;
use base_access_lists::{FBALBuilderDb, FlashblockAccessList};
use base_common_evm::{BaseContext, BaseTransaction, Builder, DefaultBase, OpSpecId};
use base_test_utils::{
    AccessListContract, ContractFactory, DEVNET_CHAIN_ID, GENESIS_GAS_LIMIT, Logic, Logic2, Proxy,
    SimpleStorage,
};
use eyre::Result;
use revm::{
    DatabaseCommit, ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv, result::ResultAndState},
    context_interface::block::BlobExcessGasAndPrice,
    database::InMemoryDB,
    interpreter::instructions::utility::IntoAddress,
    primitives::ONE_ETHER,
    state::{AccountInfo, Bytecode},
};

mod delegatecall;
mod deployment;
mod storage;
mod transfers;

/// Builds the static block environment used for access-list tests.
fn block_env() -> BlockEnv {
    BlockEnv {
        number: U256::ZERO,
        beneficiary: Address::ZERO,
        timestamp: U256::from(1),
        difficulty: U256::ZERO,
        prevrandao: Some(B256::ZERO),
        gas_limit: GENESIS_GAS_LIMIT,
        basefee: 0,
        blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
            excess_blob_gas: 0,
            blob_gasprice: 1,
        }),
    }
}

/// Builds the static cfg environment used for access-list tests.
fn cfg_env() -> CfgEnv<OpSpecId> {
    let mut cfg_env = CfgEnv::new_with_spec(OpSpecId::JOVIAN);
    cfg_env.chain_id = DEVNET_CHAIN_ID;
    cfg_env
}

/// Executes a list of transactions and builds a `FlashblockAccessList` tracking all
/// account and storage changes across all transactions.
///
/// Uses a single `FBALBuilderDb` instance that wraps the underlying `InMemoryDB`,
/// calling `set_index()` before each transaction to track which txn caused which change.
pub fn execute_txns_build_access_list(
    txs: Vec<BaseTransaction<TxEnv>>,
    acc_overrides: Option<HashMap<Address, AccountInfo>>,
    storage_overrides: Option<HashMap<Address, HashMap<U256, B256>>>,
) -> Result<FlashblockAccessList> {
    // Set up the underlying InMemoryDB with any overrides
    let mut db = InMemoryDB::default();
    if let Some(overrides) = acc_overrides {
        for (address, info) in overrides {
            db.insert_account_info(address, info);
        }
    }
    if let Some(storage) = storage_overrides {
        for (address, slots) in storage {
            for (slot, value) in slots {
                db.insert_account_storage(address, slot, U256::from_be_bytes(value.0)).unwrap();
            }
        }
    }

    // Create a single FBALBuilderDb that wraps the InMemoryDB for all transactions
    let mut fbal_db = FBALBuilderDb::new(db);
    let max_tx_index = txs.len().saturating_sub(1);

    for (i, tx) in txs.into_iter().enumerate() {
        // Set the transaction index before executing each transaction
        fbal_db.set_index(i as u64);

        let ctx =
            BaseContext::base().with_db(&mut fbal_db).with_block(block_env()).with_cfg(cfg_env());
        let mut evm = ctx.build_base();
        let ResultAndState { state, .. } = evm.transact(tx).unwrap();
        drop(evm);

        // Commit the state changes to our FBALBuilderDb
        fbal_db.commit(state);
    }

    // Finish and build the access list
    let access_list_builder = fbal_db.finish()?;
    Ok(access_list_builder.build(0, max_tx_index as u64))
}
