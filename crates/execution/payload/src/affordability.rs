//! Build-time check that a declared EIP-8130 coinbase tip is payable.

use alloy_primitives::{Address, U256};
use base_execution_eip8130::FeeCheck;
use base_execution_txpool::BasePooledTx;
use revm::Database;

/// Whether a statically decoded coinbase tip can be paid with worst-case gas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoinbaseTipAffordability;

impl CoinbaseTipAffordability {
    /// Returns `true` when `sender` and `payer` cannot cover worst-case gas plus
    /// `tip` from the balances currently in `db`.
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
        FeeCheck::validate_gas_and_tip(
            payer_balance,
            sender_balance,
            payer == sender,
            gas_limit,
            payer_auth,
            max_fee,
            tip,
        )
        .is_err()
    }

    /// Returns `true` when the transaction declares a static coinbase tip that
    /// the sender and gas payer cannot cover together with worst-case gas.
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
        Self::unaffordable_tip(
            sender,
            payer,
            tx.gas_limit(),
            payer_auth,
            tx.max_fee_per_gas(),
            tip,
            db,
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use base_common_consensus::{
        BaseTxEnvelope, Call, Eip8130Constants, Eip8130Signed, Predeploys, TxEip8130,
    };
    use base_execution_txpool::{BasePooledTransaction, BasePooledTx, LimitClass};
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
            SENDER, SENDER, 21_000, 0, 2, TIP, &mut db
        ));
    }

    #[test]
    fn funded_self_pay_covers_gas_plus_tip() {
        let mut db = InMemoryDB::default();
        // gas = 21_000 * 2 = 42_000; tip = 1_000.
        fund(&mut db, SENDER, 43_000);
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER, SENDER, 21_000, 0, 2, TIP, &mut db
        ));
    }

    #[test]
    fn self_pay_short_one_wei_is_unaffordable() {
        let mut db = InMemoryDB::default();
        fund(&mut db, SENDER, 42_999);
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER, SENDER, 21_000, 0, 2, TIP, &mut db
        ));
    }

    #[test]
    fn sponsored_tip_needs_sender_balance() {
        let mut db = InMemoryDB::default();
        fund(&mut db, PAYER, 42_000);
        fund(&mut db, SENDER, 999);
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER, PAYER, 21_000, 0, 2, TIP, &mut db
        ));
    }

    #[test]
    fn sponsored_gas_and_tip_are_affordable_when_split() {
        let mut db = InMemoryDB::default();
        fund(&mut db, PAYER, 42_000);
        fund(&mut db, SENDER, 1_000);
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER, PAYER, 21_000, 0, 2, TIP, &mut db
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
        fund(&mut db, PAYER, 42_000);
        fund(&mut db, SENDER, 1_000);
        let tx = unrecoverable_open_payer_tip();
        assert!(
            CoinbaseTipAffordability::unaffordable(&tx, 0, &mut db),
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
            !CoinbaseTipAffordability::unaffordable(&tx, 0, &mut db),
            "the classified payer covers gas and the sender covers the tip"
        );
    }

    /// A policy-gated sender's fee-vault tip reverts and is never charged, so
    /// it cannot make the transaction unaffordable.
    #[test]
    fn policy_gated_sender_tip_is_not_priced() {
        let mut db = InMemoryDB::default();
        fund(&mut db, SENDER, 1_000);
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
            CoinbaseTipAffordability::unaffordable(&tx, 0, &mut db),
            "an ungated sender short of gas plus the tip is unaffordable"
        );

        tx.set_sender_policy_target(Address::repeat_byte(0x77));
        assert!(
            !CoinbaseTipAffordability::unaffordable(&tx, 0, &mut db),
            "a gated sender's reverting tip is not priced"
        );
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
            &mut FailingDatabase
        ));
    }
}
