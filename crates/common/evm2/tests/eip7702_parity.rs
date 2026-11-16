//! Differential parity harness for the EIP-7702 set-code transaction fork gate.
//!
//! Runs the same type-4 transaction through `base-common-evm2` and the revm-based
//! `base-common-evm` reference at every Base execution upgrade. Before Isthmus (Prague) both
//! engines must reject it as an unsupported transaction type; from Isthmus on both must execute it
//! and agree on the outcome: success, gas used (before Everest), the sender nonce, and the
//! authority's delegation code hash. The authorization is pre-recovered and all fees are zero, so
//! no signing or funding is needed.

use alloy_consensus::{TxEip7702, transaction::Recovered};
use alloy_eips::eip7702::{
    Authorization, RecoveredAuthority, RecoveredAuthorization, SignedAuthorization,
};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use base_common_evm::{
    BaseSpecId as RevmBaseSpecId, BaseTransaction, BaseTransactionError, Builder, DefaultBase,
    L1BlockInfo,
};
use base_common_evm2::{BaseEvmTypes, BaseSpecId, BaseTxEnvelope};
use base_common_genesis::BaseUpgrade;
use evm2::{Evm, env::BlockEnv, ethereum::TxEnvelope, evm::InMemoryDB, registry::HandlerError};
use revm::{
    ExecuteEvm,
    context::{CfgEnv, Context, TxEnv},
    context_interface::result::{EVMError, InvalidTransaction, ResultAndState},
    database::InMemoryDB as RevmDb,
};

const SENDER: Address = Address::repeat_byte(0x11);
const TARGET: Address = Address::repeat_byte(0x22);
const AUTHORITY: Address = Address::repeat_byte(0x44);
const DELEGATE: Address = Address::with_last_byte(1);
const CHAIN_ID: u64 = 1;
const GAS_LIMIT: u64 = 100_000;

/// The observable outcome of an accepted set-code transaction.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    success: bool,
    gas_used: u64,
    sender_nonce: u64,
    authority_code_hash: B256,
}

fn authorization() -> Authorization {
    Authorization { chain_id: U256::from(CHAIN_ID), address: DELEGATE, nonce: 0 }
}

fn recovered_auth() -> RecoveredAuthorization {
    RecoveredAuthorization::new_unchecked(authorization(), RecoveredAuthority::Valid(AUTHORITY))
}

const fn enveloped() -> Bytes {
    Bytes::from_static(&[0x04])
}

/// The code hash of an EIP-7702 delegation designator (`0xef0100 || DELEGATE`).
fn delegation_code_hash() -> B256 {
    let mut designator = vec![0xef, 0x01, 0x00];
    designator.extend_from_slice(DELEGATE.as_slice());
    keccak256(designator)
}

/// Runs the set-code transaction through evm2 at `upgrade`, committing its state.
fn run_evm2(upgrade: BaseUpgrade) -> Result<Outcome, HandlerError> {
    let spec = BaseSpecId::new(upgrade);
    let mut evm = Evm::new(
        spec,
        BlockEnv::<BaseEvmTypes>::default(),
        BaseEvmTypes::tx_registry(spec),
        InMemoryDB::default(),
        BaseEvmTypes::precompiles(spec),
    );
    let tx = TxEip7702 {
        chain_id: CHAIN_ID,
        gas_limit: GAS_LIMIT,
        to: TARGET,
        authorization_list: vec![SignedAuthorization::new_unchecked(
            authorization(),
            0,
            U256::ZERO,
            U256::ZERO,
        )],
        ..Default::default()
    };
    let inner =
        TxEnvelope::Eip7702(evm2::ethereum::LazyTxEip7702::from_cached_recovered_authorizations(
            tx,
            vec![recovered_auth()],
        ));
    let envelope = BaseTxEnvelope::standard(inner, enveloped());
    let result = evm.transact(&Recovered::new_unchecked(envelope, SENDER))?.commit();

    let mut info = |address| evm.state_mut().account_info_untracked(&address).unwrap();
    Ok(Outcome {
        success: result.status,
        gas_used: result.tx_gas_used(),
        sender_nonce: info(SENDER).map(|i| i.nonce).unwrap_or_default(),
        authority_code_hash: info(AUTHORITY).map(|i| i.code_hash).unwrap_or_default(),
    })
}

/// Runs the same transaction through the revm reference at `upgrade`.
fn run_revm(
    upgrade: BaseUpgrade,
) -> Result<Outcome, EVMError<core::convert::Infallible, BaseTransactionError>> {
    let mut cfg = CfgEnv::new_with_spec(RevmBaseSpecId::new(upgrade));
    cfg.chain_id = CHAIN_ID;
    let mut evm = Context::base()
        .with_db(RevmDb::default())
        .with_chain(L1BlockInfo {
            l2_block: Some(U256::ZERO),
            operator_fee_scalar: Some(U256::ZERO),
            operator_fee_constant: Some(U256::ZERO),
            ..Default::default()
        })
        .with_cfg(cfg)
        .build_base();
    let base = TxEnv::builder()
        .tx_type(Some(4))
        .caller(SENDER)
        .chain_id(Some(CHAIN_ID))
        .kind(TxKind::Call(TARGET))
        .gas_limit(GAS_LIMIT)
        .gas_price(0)
        .gas_priority_fee(Some(0))
        .authorization_list_recovered(vec![recovered_auth()]);
    let mut tx = BaseTransaction::builder().base(base).build_fill();
    tx.enveloped_tx = Some(enveloped());
    let ResultAndState { result, state } = evm.transact(tx)?;

    Ok(Outcome {
        success: result.is_success(),
        gas_used: result.tx_gas_used(),
        sender_nonce: state.get(&SENDER).map(|a| a.info.nonce).unwrap_or_default(),
        authority_code_hash: state.get(&AUTHORITY).map(|a| a.info.code_hash).unwrap_or_default(),
    })
}

#[test]
fn set_code_transaction_fork_gate_matches_revm() {
    for upgrade in BaseUpgrade::EXECUTION_VARIANTS {
        let evm2 = run_evm2(upgrade);
        let revm = run_revm(upgrade);
        if (upgrade as u8) < (BaseUpgrade::Isthmus as u8) {
            assert!(
                matches!(evm2, Err(HandlerError::UnsupportedTransactionType(4))),
                "evm2 must reject type 4 as unsupported at {upgrade:?}, got {evm2:?}",
            );
            assert!(
                matches!(
                    revm,
                    Err(EVMError::Transaction(BaseTransactionError::Base(
                        InvalidTransaction::Eip7702NotSupported
                    )))
                ),
                "revm must reject type 4 as unsupported at {upgrade:?}, got {revm:?}",
            );
        } else {
            let evm2 = evm2.unwrap_or_else(|e| panic!("evm2 rejected type 4 at {upgrade:?}: {e}"));
            let revm =
                revm.unwrap_or_else(|e| panic!("revm rejected type 4 at {upgrade:?}: {e:?}"));
            assert!(evm2.success, "set-code tx must succeed at {upgrade:?}");
            assert_eq!(
                evm2.authority_code_hash,
                delegation_code_hash(),
                "delegation must be applied at {upgrade:?}",
            );
            // evm2 does not implement Everest gas-limit billing yet, under which revm bills the
            // full gas limit, so gas used is only compared before Everest.
            let revm = if (upgrade as u8) >= (BaseUpgrade::Everest as u8) {
                Outcome { gas_used: evm2.gas_used, ..revm }
            } else {
                revm
            };
            assert_eq!(evm2, revm, "set-code tx outcome diverged at {upgrade:?}");
        }
    }
}
