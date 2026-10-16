//! Shared fixtures and test data reused by integration tests across the Base codebase.

use std::sync::Arc;

use alloy_genesis::GenesisAccount;
use alloy_primitives::{U256, utils::Unit};
use base_execution_chainspec::BaseChainSpec;
use base_test_utils::{Account, GENESIS_GAS_LIMIT, build_test_genesis};

use crate::test_utils::TEST_ACCOUNT_BALANCE_ETH;

/// Creates a test chain spec with pre-funded test accounts.
pub fn load_chain_spec() -> Arc<BaseChainSpec> {
    let test_account_balance: U256 =
        Unit::ETHER.wei().saturating_mul(U256::from(TEST_ACCOUNT_BALANCE_ETH));

    let genesis = build_test_genesis()
        .extend_accounts(
            Account::all()
                .into_iter()
                .map(|a| {
                    (a.address(), GenesisAccount::default().with_balance(test_account_balance))
                })
                .collect::<Vec<_>>(),
        )
        .with_gas_limit(GENESIS_GAS_LIMIT);

    Arc::new(BaseChainSpec::from_genesis(genesis))
}
