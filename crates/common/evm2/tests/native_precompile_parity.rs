//! Differential parity harness for the stateful Base native precompiles.
//!
//! Runs EIP-1559 transactions that call the Base native precompiles through the evm2 engine
//! (with [`BaseEvmTypes::native_precompiles`]) and the revm reference (with its node precompile
//! set), asserting identical status, output, and gas across the fork ladder. The `TxContext`
//! getters read `tx.origin` outside an EIP-8130 transaction, so they also pin the evm2 origin
//! wiring; the fork sweep pins the Beryl/Cobalt activation gates.

use alloy_consensus::{TxEip1559, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::SolCall;
use base_common_evm::{BaseSpecId as RevmBaseSpecId, BaseTransaction, Builder, DefaultBase};
use base_common_evm2::{BaseEvmExt, BaseEvmTypes, BaseSpecId, BaseTxEnvelope};
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    ActivationRegistryStorage, ITransactionContext, NonceManagerStorage, TxContextStorage,
};
use evm2::{Evm, env::BlockEnv, ethereum::TxEnvelope, evm::InMemoryDB};
use revm::{
    ExecuteEvm,
    context::{BlockEnv as RevmBlockEnv, CfgEnv, Context, TxEnv},
    context_interface::result::ExecutionResult,
    database::InMemoryDB as RevmDb,
    state::AccountInfo as RevmAccountInfo,
};

const SENDER: Address = Address::repeat_byte(0x11);
const CHAIN_ID: u64 = 1;
const GAS_LIMIT: u64 = 200_000;
const MAX_FEE: u128 = 1_000;

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    success: bool,
    gas_used: u64,
    output: Bytes,
}

fn enveloped() -> Bytes {
    Bytes::from(vec![0x02u8; 120])
}

fn run_evm2(upgrade: BaseUpgrade, to: Address, input: &Bytes) -> Outcome {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &SENDER,
        evm2::AccountInfo { balance: U256::from(10u128.pow(18)), ..Default::default() },
    );
    let spec = BaseSpecId::new(upgrade);
    let mut evm = Evm::new(
        spec,
        BlockEnv::<BaseEvmTypes>::default(),
        BaseEvmTypes::tx_registry(),
        db,
        BaseEvmTypes::native_precompiles(spec, None),
    )
    .with_ext(BaseEvmExt { l1_fee_params_block: Some(U256::ZERO), ..Default::default() });
    let tx = TxEnvelope::Eip1559(TxEip1559 {
        chain_id: CHAIN_ID,
        gas_limit: GAS_LIMIT,
        max_fee_per_gas: MAX_FEE,
        to: TxKind::Call(to),
        input: input.clone(),
        ..Default::default()
    });
    let envelope = BaseTxEnvelope::standard(tx, enveloped());
    let result = evm.transact(&Recovered::new_unchecked(envelope, SENDER)).unwrap().commit();
    Outcome { success: result.status, gas_used: result.tx_gas_used(), output: result.output }
}

fn run_revm(upgrade: BaseUpgrade, to: Address, input: &Bytes) -> Outcome {
    let mut db = RevmDb::default();
    db.insert_account_info(
        SENDER,
        RevmAccountInfo { balance: U256::from(10u128.pow(18)), ..Default::default() },
    );
    let mut evm = Context::base()
        .with_db(db)
        .with_chain(base_common_evm::L1BlockInfo {
            l2_block: Some(U256::ZERO),
            ..Default::default()
        })
        .with_block(RevmBlockEnv::default())
        .with_cfg(CfgEnv::new_with_spec(RevmBaseSpecId::new(upgrade)))
        .build_base();
    let tx = BaseTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(SENDER)
                .chain_id(Some(CHAIN_ID))
                .kind(TxKind::Call(to))
                .data(input.clone())
                .gas_limit(GAS_LIMIT)
                .max_fee_per_gas(MAX_FEE)
                .gas_priority_fee(Some(0)),
        )
        .enveloped_tx(Some(enveloped()))
        .build_fill();
    let result = evm.transact(tx).unwrap().result;
    let output = match &result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        ExecutionResult::Revert { output, .. } => output.clone(),
        ExecutionResult::Halt { .. } => Bytes::new(),
    };
    Outcome { success: result.is_success(), gas_used: result.tx_gas_used(), output }
}

fn assert_parity(to: Address, input: Bytes) -> Vec<Outcome> {
    BaseUpgrade::EXECUTION_VARIANTS
        .into_iter()
        .filter(|upgrade| *upgrade >= BaseUpgrade::Azul)
        .map(|upgrade| {
            let evm2 = run_evm2(upgrade, to, &input);
            let revm = run_revm(upgrade, to, &input);
            assert_eq!(evm2, revm, "native precompile call to {to} diverged at {upgrade:?}");
            evm2
        })
        .collect()
}

#[test]
fn tx_context_reads_the_transaction_origin_from_cobalt() {
    let input = Bytes::from(ITransactionContext::getTransactionSenderCall {}.abi_encode());
    assert_parity(TxContextStorage::ADDRESS, input.clone());
    let cobalt = run_evm2(BaseUpgrade::Cobalt, TxContextStorage::ADDRESS, &input);
    assert!(cobalt.success);
    assert_eq!(
        cobalt.output.as_ref(),
        SENDER.into_word().as_slice(),
        "outside 8130 the sender is tx.origin"
    );
    let beryl = run_evm2(BaseUpgrade::Beryl, TxContextStorage::ADDRESS, &input);
    assert!(beryl.output.is_empty(), "TxContext is not a precompile before Cobalt");
}

#[test]
fn nonce_manager_and_activation_registry_match_across_forks() {
    assert_parity(NonceManagerStorage::ADDRESS, Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]));
    assert_parity(
        ActivationRegistryStorage::ADDRESS,
        Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]),
    );
}
