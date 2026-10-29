//! Build-time check that a declared EIP-8130 coinbase tip is payable.

use alloy_primitives::{Address, U256};
use base_common_chains::BaseUpgrade;
use base_common_consensus::Predeploys;
use base_common_evm2::L1Block;
use base_common_l1_fees::L1FeeParams;
use base_execution_eip8130::FeeCheck;
use base_execution_txpool::BasePooledTx;
use evm2::evm::DynDatabase;
use tracing::warn;

/// Whether a statically decoded coinbase tip can be paid after the payer's
/// prepay, priced with the L1 fee parameters and base fee of the block being
/// built.
#[derive(Debug, Clone)]
pub struct CoinbaseTipAffordability {
    l1_fee_params: L1FeeParams,
    upgrade: BaseUpgrade,
    base_fee: u128,
}

impl CoinbaseTipAffordability {
    /// Creates a check pricing prepay with `l1_fee_params` under `upgrade` at
    /// `base_fee`.
    pub const fn new(l1_fee_params: L1FeeParams, upgrade: BaseUpgrade, base_fee: u128) -> Self {
        Self { l1_fee_params, upgrade, base_fee }
    }

    /// Creates a check from the `L1Block` predeploy state in `db`.
    ///
    /// Must run after the block's L1 attributes deposit has executed. A failed
    /// read prices the L1 and operator fees at zero, so a transient DB error
    /// does not drop otherwise-valid candidates.
    pub fn load(
        db: &mut (impl DynDatabase + ?Sized),
        upgrade: BaseUpgrade,
        base_fee: u128,
    ) -> Self {
        let l1_fee_params = L1Block::read_fee_params(upgrade, |slot| {
            db.get_storage(&Predeploys::L1_BLOCK_INFO, &slot)
        })
        .unwrap_or_else(|error| {
            warn!(
                target: "payload_builder",
                error = %error,
                "failed to load L1 block info for coinbase tip affordability"
            );
            L1FeeParams::default()
        });
        Self::new(l1_fee_params, upgrade, base_fee)
    }

    /// Returns what execution debits the payer before the calls run: the
    /// chargeable gas at the effective gas price plus the L1 data fee and the
    /// operator fee.
    pub fn prepay(
        &self,
        encoded: &[u8],
        gas_limit: u64,
        payer_auth: u64,
        max_fee: u128,
        max_priority_fee: u128,
    ) -> U256 {
        let gas_price = FeeCheck::effective_gas_price(max_fee, max_priority_fee, self.base_fee);
        let max_gas = FeeCheck::max_chargeable_gas(gas_limit, payer_auth);
        let l1_cost = self.l1_fee_params.calculate_tx_l1_cost(encoded, self.upgrade);
        let operator_cost =
            self.l1_fee_params.operator_fee_charge(encoded, U256::from(max_gas), self.upgrade);
        FeeCheck::max_fee_charge(gas_limit, payer_auth, gas_price)
            .saturating_add(l1_cost)
            .saturating_add(operator_cost)
    }

    /// Returns `true` when `payer` cannot cover `prepay` or `sender` cannot
    /// then cover `tip` from the balances currently in `db`.
    ///
    /// A failed account read is treated as affordable so a transient DB error
    /// does not drop an otherwise-valid candidate.
    pub fn unaffordable_tip(
        sender: Address,
        payer: Address,
        prepay: U256,
        tip: U256,
        db: &mut (impl DynDatabase + ?Sized),
    ) -> bool {
        let Ok(payer_info) = db.get_account(&payer) else {
            return false;
        };
        let payer_balance = payer_info.map_or(U256::ZERO, |info| info.balance);
        let sender_balance = if payer == sender {
            payer_balance
        } else {
            let Ok(sender_info) = db.get_account(&sender) else {
                return false;
            };
            sender_info.map_or(U256::ZERO, |info| info.balance)
        };
        FeeCheck::validate_gas_and_tip(payer_balance, sender_balance, payer == sender, prepay, tip)
            .is_err()
    }

    /// Returns `true` when the transaction declares a static coinbase tip that
    /// the sender and gas payer cannot cover together with the prepay.
    ///
    /// Transactions without a statically decoded tip are treated as affordable.
    pub fn unaffordable<T: BasePooledTx>(
        &self,
        tx: &T,
        payer_auth: u64,
        db: &mut (impl DynDatabase + ?Sized),
    ) -> bool {
        let Some(signed) = tx.as_eip8130() else {
            return false;
        };
        let Some(tip) = tx.coinbase_tip() else {
            return false;
        };
        let sender = tx.sender();
        // Admission already resolved the payer (recovering it in open payer
        // mode), so reuse it rather than recovering the signature again for
        // every build attempt. Without a classification, an open payer whose
        // signature does not recover cannot be priced and is skipped.
        let payer = match tx.limit_class() {
            Some(class) => class.payer,
            None => {
                let Ok(payer) = signed.resolved_payer(sender) else {
                    return true;
                };
                payer
            }
        };
        let prepay = self.prepay(
            &tx.encoded_2718(),
            tx.gas_limit(),
            payer_auth,
            tx.max_fee_per_gas(),
            tx.max_priority_fee_per_gas().unwrap_or_default(),
        );
        Self::unaffordable_tip(sender, payer, prepay, tip, db)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use base_common_chains::BaseUpgrade;
    use base_common_consensus::{
        BaseTxEnvelope, Call, Eip8130Constants, Eip8130Signed, Predeploys, TxEip8130,
    };
    use base_common_l1_fees::L1FeeParams;
    use base_execution_txpool::{BasePooledTransaction, BasePooledTx, LimitClass};
    use evm2::{
        DatabaseError,
        bytecode::Bytecode,
        evm::{AccountInfo, DbResult, DynDatabase, InMemoryDB},
        interpreter::Word,
    };

    use super::CoinbaseTipAffordability;

    const SENDER: Address = Address::repeat_byte(0x11);
    const PAYER: Address = Address::repeat_byte(0x22);
    const TIP: U256 = U256::from_limbs([1_000, 0, 0, 0]);
    const GAS_LIMIT: u64 = 21_000;
    /// [`GAS_LIMIT`] gas at `2` wei.
    const GAS_CHARGE: U256 = U256::from_limbs([42_000, 0, 0, 0]);
    /// Equal to the test transactions' fee caps, so the effective gas price is
    /// the fee cap.
    const BASE_FEE: u128 = 2;

    #[derive(Debug, thiserror::Error)]
    #[error("test database read failed")]
    struct ReadError;

    #[derive(Debug, Default)]
    struct FailingDatabase;

    impl FailingDatabase {
        fn error() -> DatabaseError {
            DatabaseError::new(ReadError, false)
        }
    }

    impl DynDatabase for FailingDatabase {
        fn get_account(&mut self, _address: &Address) -> DbResult<Option<AccountInfo>> {
            Err(Self::error())
        }

        fn get_code_by_hash(&mut self, _code_hash: &B256) -> DbResult<Bytecode> {
            Err(Self::error())
        }

        fn get_storage(&mut self, _address: &Address, _key: &Word) -> DbResult<Word> {
            Err(Self::error())
        }

        fn get_block_hash(&mut self, _number: &Word) -> DbResult<B256> {
            Err(Self::error())
        }
    }

    fn without_l1_fees() -> CoinbaseTipAffordability {
        CoinbaseTipAffordability::new(L1FeeParams::default(), BaseUpgrade::Isthmus, BASE_FEE)
    }

    fn fund(db: &mut InMemoryDB, address: Address, balance: U256) {
        db.insert_account_info(&address, AccountInfo::default().with_balance(balance));
    }

    #[test]
    fn missing_account_cannot_cover_gas_plus_tip() {
        let mut db = InMemoryDB::default();
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER, SENDER, GAS_CHARGE, TIP, &mut db
        ));
    }

    #[test]
    fn funded_self_pay_covers_gas_plus_tip() {
        let mut db = InMemoryDB::default();
        // gas = 21_000 * 2 = 42_000; tip = 1_000.
        fund(&mut db, SENDER, U256::from(43_000u64));
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER, SENDER, GAS_CHARGE, TIP, &mut db
        ));
    }

    #[test]
    fn self_pay_short_one_wei_is_unaffordable() {
        let mut db = InMemoryDB::default();
        fund(&mut db, SENDER, U256::from(42_999u64));
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER, SENDER, GAS_CHARGE, TIP, &mut db
        ));
    }

    #[test]
    fn sponsored_tip_needs_sender_balance() {
        let mut db = InMemoryDB::default();
        fund(&mut db, PAYER, U256::from(42_000u64));
        fund(&mut db, SENDER, U256::from(999u64));
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER, PAYER, GAS_CHARGE, TIP, &mut db
        ));
    }

    #[test]
    fn sponsored_gas_and_tip_are_affordable_when_split() {
        let mut db = InMemoryDB::default();
        fund(&mut db, PAYER, U256::from(42_000u64));
        fund(&mut db, SENDER, U256::from(1_000u64));
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER, PAYER, GAS_CHARGE, TIP, &mut db
        ));
    }

    /// An open-payer transaction tipping the fee vault, whose `payer_auth` is
    /// 65 zero bytes (`v = 0`), which never recovers.
    fn unrecoverable_open_payer_tip() -> BasePooledTransaction {
        let tx = TxEip8130 {
            gas_limit: 21_000,
            max_fee_per_gas: 2,
            payer: Some(Eip8130Constants::OPEN_PAYER),
            calls: vec![vec![Call {
                to: Predeploys::SEQUENCER_FEE_VAULT,
                value: TIP,
                data: Bytes::new(),
            }]],
            ..Default::default()
        };
        let envelope = BaseTxEnvelope::Eip8130(Eip8130Signed::new(
            tx,
            Bytes::from(vec![0u8; 65]),
            Bytes::from(vec![0u8; 65]),
        ));
        let encoded_len = envelope.encode_2718_len();
        BasePooledTransaction::new(Recovered::new_unchecked(envelope, SENDER), encoded_len)
    }

    /// Block building prices an open payer from the payer admission resolved,
    /// without recovering `payer_auth` again. Without a classification the
    /// payer must be recovered, and an unrecoverable one is skipped.
    #[test]
    fn open_payer_uses_the_admission_resolved_payer() {
        let mut db = InMemoryDB::default();
        fund(&mut db, PAYER, U256::from(42_000u64));
        fund(&mut db, SENDER, U256::from(1_000u64));
        let tx = unrecoverable_open_payer_tip();
        assert!(
            without_l1_fees().unaffordable(&tx, 0, &mut db),
            "an unclassified open payer that does not recover is unaffordable"
        );

        tx.set_limit_class(LimitClass {
            sender: SENDER,
            payer: PAYER,
            classification_generation: 0,
            sender_locked: false,
            payer_locked: false,
            payer_trusted: false,
            payer_allowlisted: false,
            payer_balance: U256::from(42_000u64),
            max_cost: U256::from(42_000u64),
        });
        assert!(
            !without_l1_fees().unaffordable(&tx, 0, &mut db),
            "the classified payer covers gas and the sender covers the tip"
        );
    }

    /// A policy-gated sender's fee-vault tip reverts and is never charged, so
    /// it cannot make the transaction unaffordable.
    #[test]
    fn policy_gated_sender_tip_is_not_priced() {
        let mut db = InMemoryDB::default();
        fund(&mut db, SENDER, U256::from(1_000u64));
        let tx = unrecoverable_open_payer_tip();
        tx.set_limit_class(LimitClass {
            sender: SENDER,
            payer: SENDER,
            classification_generation: 0,
            sender_locked: false,
            payer_locked: false,
            payer_trusted: false,
            payer_allowlisted: false,
            payer_balance: U256::from(1_000u64),
            max_cost: U256::from(42_000u64),
        });
        assert!(
            without_l1_fees().unaffordable(&tx, 0, &mut db),
            "an ungated sender short of gas plus the tip is unaffordable"
        );

        tx.set_sender_policy_target(Address::repeat_byte(0x77));
        assert!(
            !without_l1_fees().unaffordable(&tx, 0, &mut db),
            "a gated sender's reverting tip is not priced"
        );
    }

    #[test]
    fn database_read_errors_fail_open() {
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            SENDER,
            GAS_CHARGE,
            TIP,
            &mut FailingDatabase
        ));
    }

    /// Non-zero L1 data and operator fee parameters.
    fn l1_fee_params_with_fees() -> L1FeeParams {
        L1FeeParams {
            l1_base_fee: U256::from(1_000_000_000u64),
            l1_base_fee_scalar: U256::from(1_000_000u64),
            l1_blob_base_fee: Some(U256::from(1u64)),
            l1_blob_base_fee_scalar: Some(U256::from(1u64)),
            operator_fee_scalar: Some(U256::from(1_000_000u64)),
            operator_fee_constant: Some(U256::from(500u64)),
            ..Default::default()
        }
    }

    /// A self-paid transaction tipping the fee vault.
    fn self_paid_tip(
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            gas_limit: GAS_LIMIT,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            calls: vec![vec![Call {
                to: Predeploys::SEQUENCER_FEE_VAULT,
                value: TIP,
                data: Bytes::new(),
            }]],
            ..Default::default()
        };
        let envelope = BaseTxEnvelope::Eip8130(Eip8130Signed::new(
            tx,
            Bytes::from(vec![0u8; 65]),
            Bytes::new(),
        ));
        let encoded_len = envelope.encode_2718_len();
        BasePooledTransaction::new(Recovered::new_unchecked(envelope, SENDER), encoded_len)
    }

    /// Execution debits a self-paying sender the L1 data and operator fees
    /// before the tip phase, so a balance of exactly gas plus the tip cannot
    /// pay the tip, and the builder must skip the transaction.
    #[test]
    fn self_pay_must_cover_l1_and_operator_fees_before_the_tip() {
        let upgrade = BaseUpgrade::Isthmus;
        let tx = self_paid_tip(BASE_FEE, BASE_FEE);
        let encoded = tx.encoded_2718();
        let params = l1_fee_params_with_fees();
        let l1_fee = params.calculate_tx_l1_cost(encoded, upgrade);
        let operator_fee = params.operator_fee_charge(encoded, U256::from(GAS_LIMIT), upgrade);
        assert!(!l1_fee.is_zero() && !operator_fee.is_zero(), "fixture must charge both fees");
        let affordability = CoinbaseTipAffordability::new(params, upgrade, BASE_FEE);

        let mut db = InMemoryDB::default();
        let gas_plus_tip = GAS_CHARGE + TIP;
        fund(&mut db, SENDER, gas_plus_tip);
        assert!(
            affordability.unaffordable(&tx, 0, &mut db),
            "gas plus the tip leaves nothing for the L1 and operator fees"
        );

        let full = gas_plus_tip + l1_fee + operator_fee;
        fund(&mut db, SENDER, full);
        assert!(!affordability.unaffordable(&tx, 0, &mut db));
        fund(&mut db, SENDER, full - U256::from(1u64));
        assert!(affordability.unaffordable(&tx, 0, &mut db));
    }

    /// Execution prepays gas at the block's effective gas price, so a sender
    /// funded for that and the tip is affordable even below its fee cap.
    #[test]
    fn gas_is_priced_at_the_effective_gas_price() {
        // min(max_fee = 10, base_fee + priority = 2 + 1) = 3 wei per gas.
        let tx = self_paid_tip(10, 1);
        let mut db = InMemoryDB::default();
        let balance = U256::from(GAS_LIMIT * 3) + TIP;
        fund(&mut db, SENDER, balance);
        assert!(!without_l1_fees().unaffordable(&tx, 0, &mut db));
        fund(&mut db, SENDER, balance - U256::from(1u64));
        assert!(without_l1_fees().unaffordable(&tx, 0, &mut db));
    }
}
