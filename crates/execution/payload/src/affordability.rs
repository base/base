//! Build-time check that a declared EIP-8130 coinbase tip is payable.

use alloy_primitives::{Address, U256};
use base_common_consensus::CoinbaseTip;
use base_execution_eip8130::FeeCheck;
use base_execution_txpool::BasePooledTx;
use revm::Database;

/// Whether a statically decoded coinbase tip can be paid with worst-case gas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoinbaseTipAffordability;

impl CoinbaseTipAffordability {
    /// Returns `true` when `sender` and `payer` cannot cover worst-case gas plus
    /// the phase-0 self-call from the balances currently in `db`.
    ///
    /// `tip` is the inner vault transfer. `call_value` is the protocol call's
    /// value on that same self-call. The value is checked against the sender
    /// and then credited back to the sender before the vault transfer, so both
    /// holds observe one balance and the sender must cover the larger of them.
    ///
    /// A failed account read is treated as affordable so a transient DB error
    /// does not drop an otherwise-valid candidate.
    pub fn unaffordable_tip<DB: Database>(
        sender: Address,
        payer: Address,
        gas_limit: u64,
        payer_auth: u64,
        max_fee: u128,
        tip: U256,
        call_value: U256,
        db: &mut DB,
    ) -> bool {
        let Ok(payer_info) = db.basic(payer) else {
            return false;
        };
        let payer_balance = payer_info.map_or(U256::ZERO, |info| info.balance);
        let sender_balance = if payer == sender {
            payer_balance
        } else {
            let Ok(sender_info) = db.basic(sender) else {
                return false;
            };
            sender_info.map_or(U256::ZERO, |info| info.balance)
        };
        // Self-call: `call_value` returns to the sender before the tip leaves,
        // so the two amounts do not add.
        let sender_hold = tip.max(call_value);
        FeeCheck::validate_gas_and_tip(
            payer_balance,
            sender_balance,
            payer == sender,
            gas_limit,
            payer_auth,
            max_fee,
            sender_hold,
        )
        .is_err()
    }

    /// Returns `true` when the transaction declares a static coinbase tip that
    /// the sender and gas payer cannot cover together with worst-case gas and
    /// the phase-0 self-call value.
    ///
    /// Transactions without a statically decoded tip are treated as affordable.
    pub fn unaffordable<T, DB>(tx: &T, payer_auth: u64, db: &mut DB) -> bool
    where
        T: BasePooledTx,
        DB: Database,
    {
        let Some(signed) = tx.as_eip8130() else {
            return false;
        };
        let body = signed.tx();
        let Some(tip) = CoinbaseTip::decode(body, tx.sender()) else {
            return false;
        };
        // `decode` only succeeds for a single phase-0 self-call, which is the
        // value the sender must hold alongside the tip.
        let Some(call_value) = body.calls.first().and_then(|phase| {
            let [call] = phase.as_slice() else {
                return None;
            };
            Some(call.value)
        }) else {
            return false;
        };
        let sender = tx.sender();
        let payer = body.payer.unwrap_or(sender);
        Self::unaffordable_tip(
            sender,
            payer,
            tx.gas_limit(),
            payer_auth,
            tx.max_fee_per_gas(),
            tip,
            call_value,
            db,
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use revm::{
        Database,
        database::InMemoryDB,
        database_interface::DBErrorMarker,
        state::{AccountInfo, Bytecode},
    };

    use super::CoinbaseTipAffordability;

    const SENDER: Address = Address::repeat_byte(0x11);
    const PAYER: Address = Address::repeat_byte(0x22);
    const TIP: U256 = U256::from_limbs([1_000, 0, 0, 0]);

    #[derive(Debug, thiserror::Error)]
    #[error("test database read failed")]
    struct ReadError;

    impl DBErrorMarker for ReadError {}

    #[derive(Debug, Default)]
    struct FailingDatabase;

    impl Database for FailingDatabase {
        type Error = ReadError;

        fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(ReadError)
        }

        fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Err(ReadError)
        }

        fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Err(ReadError)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Err(ReadError)
        }
    }

    fn fund(db: &mut InMemoryDB, address: Address, balance: u64) {
        db.insert_account_info(
            address,
            AccountInfo { balance: U256::from(balance), ..Default::default() },
        );
    }

    #[test]
    fn missing_account_cannot_cover_gas_plus_tip() {
        let mut db = InMemoryDB::default();
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            SENDER,
            21_000,
            0,
            2,
            TIP,
            U256::ZERO,
            &mut db
        ));
    }

    #[test]
    fn funded_self_pay_covers_gas_plus_tip() {
        let mut db = InMemoryDB::default();
        // gas = 21_000 * 2 = 42_000; tip = 1_000.
        fund(&mut db, SENDER, 43_000);
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            SENDER,
            21_000,
            0,
            2,
            TIP,
            U256::ZERO,
            &mut db
        ));
    }

    #[test]
    fn self_pay_short_one_wei_is_unaffordable() {
        let mut db = InMemoryDB::default();
        fund(&mut db, SENDER, 42_999);
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            SENDER,
            21_000,
            0,
            2,
            TIP,
            U256::ZERO,
            &mut db
        ));
    }

    #[test]
    fn sponsored_tip_needs_sender_balance() {
        let mut db = InMemoryDB::default();
        fund(&mut db, PAYER, 42_000);
        fund(&mut db, SENDER, 999);
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            PAYER,
            21_000,
            0,
            2,
            TIP,
            U256::ZERO,
            &mut db
        ));
    }

    #[test]
    fn sponsored_gas_and_tip_are_affordable_when_split() {
        let mut db = InMemoryDB::default();
        fund(&mut db, PAYER, 42_000);
        fund(&mut db, SENDER, 1_000);
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            PAYER,
            21_000,
            0,
            2,
            TIP,
            U256::ZERO,
            &mut db
        ));
    }

    #[test]
    fn self_call_value_within_the_tip_does_not_raise_the_hold() {
        let mut db = InMemoryDB::default();
        // gas = 42_000; tip = 1_000; call value = 500. The self-call credits
        // the value back before the vault transfer, so gas + tip still covers it.
        fund(&mut db, SENDER, 43_000);
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            SENDER,
            21_000,
            0,
            2,
            TIP,
            U256::from(500u64),
            &mut db
        ));
    }

    #[test]
    fn self_call_value_above_the_tip_is_unaffordable() {
        let mut db = InMemoryDB::default();
        // gas + tip = 43_000, but the self-call checks 1_500 before the tip moves.
        fund(&mut db, SENDER, 43_000);
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            SENDER,
            21_000,
            0,
            2,
            TIP,
            U256::from(1_500u64),
            &mut db
        ));
    }

    #[test]
    fn self_call_value_above_the_tip_is_affordable_when_balance_covers_it() {
        let mut db = InMemoryDB::default();
        // gas = 42_000; call value = 1_500 > tip.
        fund(&mut db, SENDER, 43_500);
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            SENDER,
            21_000,
            0,
            2,
            TIP,
            U256::from(1_500u64),
            &mut db
        ));
    }

    #[test]
    fn sponsored_sender_must_cover_call_value_when_it_exceeds_the_tip() {
        let mut db = InMemoryDB::default();
        fund(&mut db, PAYER, 42_000);
        fund(&mut db, SENDER, 1_000);
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            PAYER,
            21_000,
            0,
            2,
            TIP,
            U256::from(1_500u64),
            &mut db
        ));
    }

    #[test]
    fn database_read_errors_fail_open() {
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            SENDER,
            21_000,
            0,
            2,
            TIP,
            U256::ZERO,
            &mut FailingDatabase
        ));
    }
}
