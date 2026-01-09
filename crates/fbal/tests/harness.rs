//! Tests for ensuring the access list is built properly

use std::{collections::HashMap, sync::Arc};

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, U256};
use alloy_sol_macro::sol;
use base_fbal::{FBALBuilderDb, FlashblockAccessList, FlashblockAccessListBuilder};
use eyre::Result;
use op_revm::OpTransaction;
use reth_evm::{ConfigureEvm, Evm};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::OpEvmConfig;
use revm::{
    DatabaseCommit,
    context::{TxEnv, result::ResultAndState},
    database::InMemoryDB,
    state::AccountInfo,
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

sol!(
    #[sol(rpc)]
    Proxy,
    concat!(env!("CARGO_MANIFEST_DIR"), "/../test-utils/contracts/out/Proxy.sol/Proxy.json")
);

sol!(
    #[sol(rpc)]
    Logic,
    concat!(env!("CARGO_MANIFEST_DIR"), "/../test-utils/contracts/out/Proxy.sol/Logic.json")
);

sol!(
    #[sol(rpc)]
    Logic2,
    concat!(env!("CARGO_MANIFEST_DIR"), "/../test-utils/contracts/out/Proxy.sol/Logic2.json")
);

pub const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;

fn execute_txns_build_access_list(
    txs: Vec<OpTransaction<TxEnv>>,
    acc_overrides: Option<HashMap<Address, AccountInfo>>,
    storage_overrides: Option<HashMap<Address, HashMap<U256, B256>>>,
) -> Result<FlashblockAccessList> {
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
    if let Some(storage) = storage_overrides {
        for (address, slots) in storage {
            for (slot, value) in slots {
                db.insert_account_storage(address, slot, U256::from_be_bytes(value.0)).unwrap();
            }
        }
    }

    let mut access_list_builder = FlashblockAccessListBuilder::new();

    let max_tx_index = txs.len() - 1;
    for tx in txs.into_iter() {
        let mut fbal_db = FBALBuilderDb::new(db);

        let evm_env = evm_config.evm_env(&header).unwrap();
        let mut evm = evm_config.evm_with_env(fbal_db, evm_env);
        let ResultAndState { state, .. } = evm.transact(tx).unwrap();

        evm.db_mut().commit(state);
        fbal_db = evm.into_db();

        let (tx_list, inner_db) = fbal_db.finish()?;
        db = inner_db;
        access_list_builder.merge(tx_list);
    }

    Ok(access_list_builder.build(0, max_tx_index as u64))
}
