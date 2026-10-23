//! End-to-end native B20 gas accounting through the production Base EVM.

use alloy_evm::Evm;
use alloy_primitives::{Address, B256, U256, bytes, keccak256};
use alloy_sol_types::SolCall;
use base_common_precompiles::{B20Variant, IB20};
use revm::{
    Context,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::InMemoryDB,
    state::{AccountInfo, Bytecode},
};

use crate::{BaseSpecId, BaseTransaction, BaseUpgrade, Builder, DefaultBase, L1BlockInfo};

fn b20_transfer(
    upgrade: BaseUpgrade,
    existing: bool,
    gas_limit: u64,
) -> revm::context_interface::result::ResultGas {
    let caller = Address::from([0x11; 20]);
    let recipient = Address::from([0x22; 20]);
    let token = B20Variant::Asset.compute_address(caller, B256::ZERO).0;
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        caller,
        AccountInfo { balance: U256::from(10u64).pow(U256::from(18)), ..Default::default() },
    );
    db.insert_account_info(
        token,
        AccountInfo {
            nonce: 1,
            code: Some(Bytecode::new_legacy(bytes!("EF"))),
            ..Default::default()
        },
    );
    let root = U256::from_be_slice(&alloy_primitives::hex!(
        "c78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000"
    ));
    for (address, balance) in [(caller, 100u64), (recipient, u64::from(existing))] {
        let mut key = [0u8; 64];
        key[12..32].copy_from_slice(address.as_slice());
        key[32..].copy_from_slice(&(root + U256::from(4)).to_be_bytes::<32>());
        db.insert_account_storage(
            token,
            U256::from_be_bytes(keccak256(key).0),
            U256::from(balance),
        )
        .unwrap();
    }
    let mut cfg = CfgEnv::new_with_spec(BaseSpecId::new(upgrade));
    cfg.tx_gas_limit_cap = Some(1 << 24);
    let mut evm = Context::base()
        .with_db(db)
        .with_cfg(cfg)
        .with_block(BlockEnv { gas_limit: 40_000_000, ..Default::default() })
        .with_chain(L1BlockInfo {
            l2_block: Some(U256::ZERO),
            operator_fee_scalar: Some(U256::ZERO),
            operator_fee_constant: Some(U256::ZERO),
            ..Default::default()
        })
        .build_base()
        .with_inspector(revm::inspector::NoOpInspector);
    let tx =
        BaseTransaction::builder()
            .base(
                TxEnv::builder().caller(caller).kind(token.into()).gas_limit(gas_limit).data(
                    IB20::transferCall { to: recipient, amount: U256::ONE }.abi_encode().into(),
                ),
            )
            .enveloped_tx(Some(bytes!("FACADE")))
            .build_fill();
    let result = evm.transact(tx).expect("native transfer must execute").result;
    assert!(result.is_success(), "native transfer must succeed: {result:?}");
    *result.gas()
}

#[test]
fn native_amsterdam_b20_exact_gas_and_limit_invariance() {
    for (existing, receipt, state) in [(false, 143_966, 97_920), (true, 46_046, 0)] {
        for gas_limit in [200_000, 26_052_844] {
            let gas = b20_transfer(BaseUpgrade::Denim, existing, gas_limit);
            assert_eq!(
                gas.tx_gas_used(),
                receipt,
                "receipt must charge regular plus state, independent of declared gas; existing={existing}, limit={gas_limit}"
            );
            assert_eq!(
                gas.block_regular_gas_used(),
                46_046,
                "block regular gas must exclude state gas; existing={existing}, limit={gas_limit}"
            );
            assert_eq!(
                gas.block_state_gas_used(),
                state,
                "only a fresh recipient balance slot creates state; existing={existing}, limit={gas_limit}"
            );
        }
    }
}

#[test]
fn native_amsterdam_fresh_estimate_against_existing_balance() {
    let gas = b20_transfer(BaseUpgrade::Denim, true, 26_052_844);
    assert_eq!(
        gas.tx_gas_used(),
        46_046,
        "a fresh-estimated budget landing on an existing slot must not reproduce the erroneous 9,307,274 charge"
    );
    assert_eq!(gas.block_state_gas_used(), 0, "estimation headroom is not state gas consumed");
}

#[test]
fn native_pre_amsterdam_b20_exact_gas_unchanged() {
    for (existing, expected) in [(false, 54_746), (true, 37_646)] {
        let gas = b20_transfer(BaseUpgrade::Cobalt, existing, 200_000);
        assert_eq!(
            gas.tx_gas_used(),
            expected,
            "pre-Amsterdam native transfer receipt is unchanged; existing={existing}"
        );
        assert_eq!(
            gas.block_regular_gas_used(),
            expected,
            "pre-Amsterdam gas is entirely regular; existing={existing}"
        );
        assert_eq!(gas.block_state_gas_used(), 0, "pre-Amsterdam has no state-gas charge");
    }
}
