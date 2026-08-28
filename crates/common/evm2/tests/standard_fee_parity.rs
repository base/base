//! Differential parity harness for standard-transaction fee distribution.
//!
//! Runs the same EIP-1559 transaction through the `base-common-evm2` handler hooks and the
//! revm-based `base-common-evm` reference, asserting the two engines agree on the fee outcome:
//! success, gas used, and the post-state balances of the caller, the coinbase, and the three
//! OP-stack fee vaults (L1 / base fee / operator fee).
//!
//! The transaction fields (from the decoded tx) drive execution and the same arbitrary
//! enveloped bytes drive the L1 data-fee byte count on both sides, so no signing is needed.
//! Swept across Ecotone, Fjord, Isthmus, and Jovian to exercise each fee branch (linear vs
//! `FastLZ` L1 cost, and the Isthmus/Jovian operator-fee formulas).

use std::collections::BTreeMap;

use alloy_consensus::{TxEip1559, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use base_common_consensus::Predeploys;
use base_common_evm::{
    BaseSpecId as RevmBaseSpecId, BaseTransaction, Builder, DefaultBase, L1BlockInfo,
};
use base_common_evm2::{BaseEvmTypes, BaseSpecId, BaseTxEnvelope};
use base_common_genesis::BaseUpgrade;
use base_common_l1_fees::L1FeeParams;
use evm2::{Evm, Precompiles, env::BlockEnv, ethereum::TxEnvelope, evm::InMemoryDB};
use revm::{
    ExecuteEvm,
    context::{BlockEnv as RevmBlockEnv, CfgEnv, Context, TxEnv},
    context_interface::result::ResultAndState,
    database::InMemoryDB as RevmDb,
    state::AccountInfo as RevmAccountInfo,
};

const SENDER: Address = Address::repeat_byte(0x11);
const TARGET: Address = Address::repeat_byte(0x22);
const COINBASE: Address = Address::repeat_byte(0x33);
const CHAIN_ID: u64 = 1;
const GAS_LIMIT: u64 = 100_000;
const MAX_FEE: u128 = 1_000;
const PRIORITY_FEE: u128 = 100;
const BASEFEE: u64 = 500;
const L1_BASE_FEE: u64 = 1_000_000_000;
const L1_BASE_FEE_SCALAR: u64 = 1_000;
const OPERATOR_FEE_SCALAR: u64 = 2_000;
const OPERATOR_FEE_CONSTANT: u64 = 7;
// Ecotone (linear L1 cost), Fjord (FastLZ-estimated L1 cost), Isthmus (operator fee), and
// Jovian (operator fee × multiplier) — exercising each distinct fee branch.
const FORKS: [BaseUpgrade; 4] =
    [BaseUpgrade::Ecotone, BaseUpgrade::Fjord, BaseUpgrade::Isthmus, BaseUpgrade::Jovian];

/// Arbitrary EIP-2718-shaped bytes priced for the L1 data fee (same on both engines).
fn enveloped() -> Bytes {
    Bytes::from(vec![0x02u8; 120])
}

/// The fee accounts whose post-state is compared across engines.
const fn fee_accounts() -> [Address; 5] {
    [
        SENDER,
        COINBASE,
        Predeploys::L1_FEE_VAULT,
        Predeploys::BASE_FEE_VAULT,
        Predeploys::OPERATOR_FEE_VAULT,
    ]
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    success: bool,
    gas_used: u64,
    balances: BTreeMap<Address, U256>,
}

fn l1_fee_params() -> L1FeeParams {
    L1FeeParams {
        l1_base_fee: U256::from(L1_BASE_FEE),
        l1_base_fee_scalar: U256::from(L1_BASE_FEE_SCALAR),
        operator_fee_scalar: Some(U256::from(OPERATOR_FEE_SCALAR)),
        operator_fee_constant: Some(U256::from(OPERATOR_FEE_CONSTANT)),
        ..Default::default()
    }
}

/// Runs the EIP-1559 transfer through evm2 at `upgrade` and captures the fee outcome.
fn run_evm2(upgrade: BaseUpgrade) -> Outcome {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &SENDER,
        evm2::AccountInfo { balance: U256::from(10u128.pow(18)), nonce: 0, ..Default::default() },
    );
    let spec = BaseSpecId::new(upgrade);
    let block = BlockEnv::<BaseEvmTypes> {
        beneficiary: COINBASE,
        basefee: U256::from(BASEFEE),
        ext: l1_fee_params(),
        ..Default::default()
    };
    let mut evm =
        Evm::new(spec, block, BaseEvmTypes::tx_registry(), db, Precompiles::base(spec.into()));

    let tx = TxEnvelope::Eip1559(TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: GAS_LIMIT,
        max_fee_per_gas: MAX_FEE,
        max_priority_fee_per_gas: PRIORITY_FEE,
        to: TxKind::Call(TARGET),
        value: U256::ZERO,
        input: Bytes::new(),
        access_list: Default::default(),
    });
    let envelope = BaseTxEnvelope::standard(tx, enveloped());
    let result = evm.transact(&Recovered::new_unchecked(envelope, SENDER)).unwrap().commit();

    let balances = fee_accounts()
        .into_iter()
        .map(|address| {
            let balance = evm
                .state_mut()
                .account_info_untracked(&address)
                .unwrap()
                .map(|info| info.balance)
                .unwrap_or_default();
            (address, balance)
        })
        .collect();

    Outcome { success: result.status, gas_used: result.tx_gas_used(), balances }
}

/// Runs the same transfer through the revm reference at `upgrade`.
fn run_revm(upgrade: BaseUpgrade) -> Outcome {
    let mut db = RevmDb::default();
    db.insert_account_info(
        SENDER,
        RevmAccountInfo { balance: U256::from(10u128.pow(18)), nonce: 0, ..Default::default() },
    );
    let params = l1_fee_params();
    let ctx = Context::base()
        .with_db(db)
        .with_chain(L1BlockInfo {
            l2_block: Some(U256::ZERO),
            l1_base_fee: params.l1_base_fee,
            l1_base_fee_scalar: params.l1_base_fee_scalar,
            operator_fee_scalar: params.operator_fee_scalar,
            operator_fee_constant: params.operator_fee_constant,
            ..Default::default()
        })
        .with_block(RevmBlockEnv { basefee: BASEFEE, beneficiary: COINBASE, ..Default::default() })
        .with_cfg(CfgEnv::new_with_spec(RevmBaseSpecId::new(upgrade)));
    let mut evm = ctx.build_base();

    let tx = BaseTransaction::builder()
        .base(
            // Leave tx_type unset: the builder infers EIP-1559 from the priority fee, and (unlike
            // an explicit type) build_fill then keeps the enveloped bytes we provide instead of
            // overriding them with a `[0x00]` placeholder.
            TxEnv::builder()
                .caller(SENDER)
                .nonce(0)
                .chain_id(Some(CHAIN_ID))
                .kind(TxKind::Call(TARGET))
                .gas_limit(GAS_LIMIT)
                .max_fee_per_gas(MAX_FEE)
                .gas_priority_fee(Some(PRIORITY_FEE)),
        )
        .enveloped_tx(Some(enveloped()))
        .build_fill();
    let ResultAndState { result, state } = evm.transact(tx).unwrap();

    let balances = fee_accounts()
        .into_iter()
        .map(|address| {
            let balance = state.get(&address).map(|a| a.info.balance).unwrap_or_default();
            (address, balance)
        })
        .collect();

    Outcome { success: result.is_success(), gas_used: result.tx_gas_used(), balances }
}

#[test]
fn standard_transaction_fee_distribution_matches_revm() {
    for upgrade in FORKS {
        let evm2 = run_evm2(upgrade);
        let revm = run_revm(upgrade);
        assert_eq!(evm2, revm, "standard-tx fee distribution diverged at {upgrade:?}");
    }
}
