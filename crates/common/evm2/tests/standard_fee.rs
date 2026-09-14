//! Standalone standard-transaction fee tests for `base-common-evm2`.
//!
//! Unlike [`standard_fee_parity`](standard_fee_parity.rs), these tests do not compare against the
//! revm-based `base-common-evm` reference: they assert evm2's fee *routing* directly, computing the
//! expected fees from the engine-neutral [`L1FeeParams`] (the fee-math source of truth, which has
//! its own unit tests and survives the eventual removal of `base-common-evm`). They therefore keep
//! standard-transaction fee coverage once the differential harness is deleted.
//!
//! Swept across Ecotone, Fjord, Isthmus, and Jovian to exercise each L1-cost branch (linear vs
//! `FastLZ`) and the Isthmus/Jovian operator-fee formulas.

use alloy_consensus::{TxEip1559, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use base_common_consensus::Predeploys;
use base_common_evm2::{BaseEvmTypes, BaseSpecId, BaseTxEnvelope};
use base_common_genesis::BaseUpgrade;
use base_common_l1_fees::L1FeeParams;
use evm2::{Evm, Precompiles, env::BlockEnv, ethereum::TxEnvelope, evm::InMemoryDB};

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

/// Arbitrary EIP-2718-shaped bytes that drive the L1 data-fee byte count.
fn enveloped() -> Bytes {
    Bytes::from(vec![0x02u8; 120])
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

/// The EIP-1559 transfer exercised by the tests, paired with its enveloped bytes.
fn eip1559_envelope() -> BaseTxEnvelope {
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
    BaseTxEnvelope::standard(tx, enveloped())
}

/// Builds an evm2 instance at `upgrade` with `SENDER` funded to `balance` and `COINBASE` as the
/// block beneficiary.
fn build_evm2(upgrade: BaseUpgrade, balance: U256) -> Evm<'static, BaseEvmTypes> {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&SENDER, evm2::AccountInfo { balance, nonce: 0, ..Default::default() });
    let spec = BaseSpecId::new(upgrade);
    let block = BlockEnv::<BaseEvmTypes> {
        beneficiary: COINBASE,
        basefee: U256::from(BASEFEE),
        ext: l1_fee_params(),
        ..Default::default()
    };
    Evm::new(spec, block, BaseEvmTypes::tx_registry(), db, Precompiles::base(spec.into()))
}

fn balance(evm: &mut Evm<'static, BaseEvmTypes>, addr: Address) -> U256 {
    evm.state_mut()
        .account_info_untracked(&addr)
        .unwrap()
        .map(|info| info.balance)
        .unwrap_or_default()
}

/// Isthmus and later collect the operator fee; earlier forks do not.
const fn operator_fee_active(upgrade: BaseUpgrade) -> bool {
    (upgrade as u8) >= (BaseUpgrade::Isthmus as u8)
}

#[test]
fn fee_distribution_routes_each_fee_to_its_vault() {
    let params = l1_fee_params();
    for upgrade in FORKS {
        let initial = U256::from(10u128.pow(18));
        let mut evm = build_evm2(upgrade, initial);
        let result =
            evm.transact(&Recovered::new_unchecked(eip1559_envelope(), SENDER)).unwrap().commit();
        assert!(result.status, "standard tx succeeds at {upgrade:?}");
        let gas_used = U256::from(result.tx_gas_used());

        // Expected fees, computed independently from the surviving L1FeeParams. This guards evm2's
        // routing (which sink each fee lands in), not the fee math itself.
        let expected_l1 = params.calculate_tx_l1_cost(&enveloped(), upgrade);
        let expected_base = U256::from(BASEFEE) * gas_used;
        // Effective priority = min(max_priority, max_fee - basefee) = 100 here.
        let expected_priority = U256::from(PRIORITY_FEE) * gas_used;
        let expected_operator = if operator_fee_active(upgrade) {
            params.operator_fee_charge(&enveloped(), gas_used, upgrade)
        } else {
            U256::ZERO
        };

        assert_eq!(
            balance(&mut evm, Predeploys::L1_FEE_VAULT),
            expected_l1,
            "L1 data fee vault @ {upgrade:?}",
        );
        assert_eq!(
            balance(&mut evm, Predeploys::BASE_FEE_VAULT),
            expected_base,
            "base fee vault @ {upgrade:?}",
        );
        assert_eq!(
            balance(&mut evm, COINBASE),
            expected_priority,
            "coinbase priority fee @ {upgrade:?}",
        );
        assert_eq!(
            balance(&mut evm, Predeploys::OPERATOR_FEE_VAULT),
            expected_operator,
            "operator fee vault @ {upgrade:?}",
        );

        // Conservation: everything debited from the caller lands in exactly these sinks (the
        // transfer value is zero and the empty target is untouched), so nothing is dropped or
        // misrouted.
        let debit = initial - balance(&mut evm, SENDER);
        assert_eq!(
            debit,
            expected_l1 + expected_base + expected_priority + expected_operator,
            "no fee dropped or misrouted @ {upgrade:?}",
        );
    }
}

#[test]
fn operator_fee_is_gated_by_isthmus_and_scaled_by_jovian() {
    let params = l1_fee_params();

    // Pre-Isthmus: no operator fee is collected even though the scalars are configured.
    let mut pre = build_evm2(BaseUpgrade::Fjord, U256::from(10u128.pow(18)));
    let _ = pre.transact(&Recovered::new_unchecked(eip1559_envelope(), SENDER)).unwrap().commit();
    assert_eq!(
        balance(&mut pre, Predeploys::OPERATOR_FEE_VAULT),
        U256::ZERO,
        "operator fee is not collected before Isthmus",
    );

    // Isthmus vs Jovian: the identical transaction uses the same gas, but Jovian applies the
    // operator-fee multiplier, so its operator fee is strictly larger and matches the Jovian
    // formula.
    let mut ist = build_evm2(BaseUpgrade::Isthmus, U256::from(10u128.pow(18)));
    let ist_gas = U256::from(
        ist.transact(&Recovered::new_unchecked(eip1559_envelope(), SENDER))
            .unwrap()
            .commit()
            .tx_gas_used(),
    );
    let mut jov = build_evm2(BaseUpgrade::Jovian, U256::from(10u128.pow(18)));
    let jov_gas = U256::from(
        jov.transact(&Recovered::new_unchecked(eip1559_envelope(), SENDER))
            .unwrap()
            .commit()
            .tx_gas_used(),
    );

    let ist_op = balance(&mut ist, Predeploys::OPERATOR_FEE_VAULT);
    let jov_op = balance(&mut jov, Predeploys::OPERATOR_FEE_VAULT);
    assert_eq!(ist_op, params.operator_fee_charge(&enveloped(), ist_gas, BaseUpgrade::Isthmus));
    assert_eq!(jov_op, params.operator_fee_charge(&enveloped(), jov_gas, BaseUpgrade::Jovian));
    assert!(jov_op > ist_op, "the Jovian multiplier increases the operator fee");
}

#[test]
fn underfunded_caller_is_rejected_not_wrapped() {
    // Exactly the max gas cost (gas_limit * max_fee), with zero value: enough to clear the
    // framework's sender validation, but nothing left for the L1 or operator fee charged on top.
    // Without the affordability check, charge_upfront's saturating subtraction would wrap into a
    // spurious balance instead of failing.
    let balance_before = U256::from(GAS_LIMIT) * U256::from(MAX_FEE);
    let mut evm = build_evm2(BaseUpgrade::Isthmus, balance_before);

    let rejected = evm.transact(&Recovered::new_unchecked(eip1559_envelope(), SENDER)).is_err();
    assert!(rejected, "underfunded caller must be rejected, not charged");
    assert_eq!(
        balance(&mut evm, SENDER),
        balance_before,
        "a rejected tx must leave the caller balance untouched",
    );
}
