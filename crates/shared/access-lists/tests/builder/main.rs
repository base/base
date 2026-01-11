//! Tests for ensuring the access list is built properly

use std::collections::HashMap;

use alloy_consensus::Header;
pub use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_sol_macro::sol;
pub use alloy_sol_types::SolCall;
use base_access_lists::FBALBuilderDb;
pub use base_access_lists::FlashblockAccessList;
use base_test_utils::load_chain_spec;
pub use eyre::Result;
pub use op_revm::OpTransaction;
use reth_evm::{ConfigureEvm, Evm};
use reth_optimism_evm::OpEvmConfig;
use revm::{DatabaseCommit, context::result::ResultAndState, database::InMemoryDB};
pub use revm::{
    context::TxEnv,
    interpreter::instructions::utility::IntoAddress,
    primitives::ONE_ETHER,
    state::{AccountInfo, Bytecode},
};

mod delegatecall;
mod deployment;
mod storage;
mod transfers;

sol!(
    #[sol(rpc)]
    AccessListContract,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../client/test-utils/contracts/out/AccessList.sol/AccessList.json"
    )
);

sol!(
    #[sol(rpc)]
    ContractFactory,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../client/test-utils/contracts/out/ContractFactory.sol/ContractFactory.json"
    )
);

sol!(
    #[sol(rpc)]
    SimpleStorage,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../client/test-utils/contracts/out/ContractFactory.sol/SimpleStorage.json"
    )
);

sol!(
    #[sol(rpc)]
    Proxy,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../client/test-utils/contracts/out/Proxy.sol/Proxy.json"
    )
);

sol!(
    #[sol(rpc)]
    Logic,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../client/test-utils/contracts/out/Proxy.sol/Logic.json"
    )
);

sol!(
    #[sol(rpc)]
    Logic2,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../client/test-utils/contracts/out/Proxy.sol/Logic2.json"
    )
);

/// Chain ID for Base Sepolia
pub const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;

/// Executes a list of transactions and builds a FlashblockAccessList tracking all
/// account and storage changes across all transactions.
///
/// Uses a single FBALBuilderDb instance that wraps the underlying InMemoryDB,
/// calling set_index() before each transaction to track which txn caused which change.
pub fn execute_txns_build_access_list(
    txs: Vec<OpTransaction<TxEnv>>,
    acc_overrides: Option<HashMap<Address, AccountInfo>>,
    storage_overrides: Option<HashMap<Address, HashMap<U256, B256>>>,
) -> Result<FlashblockAccessList> {
    let chain_spec = load_chain_spec();
    let evm_config = OpEvmConfig::optimism(chain_spec.clone());
    let header = Header { base_fee_per_gas: Some(0), ..chain_spec.genesis_header().clone() };

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

        let evm_env = evm_config.evm_env(&header).unwrap();
        let mut evm = evm_config.evm_with_env(&mut fbal_db, evm_env);
        let ResultAndState { state, .. } = evm.transact(tx).unwrap();

        // Commit the state changes to our FBALBuilderDb
        fbal_db.commit(state);
    }

    // Finish and build the access list
    let access_list_builder = fbal_db.finish()?;
    Ok(access_list_builder.build(0, max_tx_index as u64))
}
