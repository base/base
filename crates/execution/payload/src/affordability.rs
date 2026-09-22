//! Build-time EIP-8130 gas (and optional coinbase tip) affordability check.
//!
//! Payer and sender addresses come from the transaction body and the recovered
//! sender. This path must not verify signatures: the native builder previously
//! learned insolvency only inside execution, after k1 payer authentication.

use alloy_primitives::{Address, U256};
use base_common_consensus::CoinbaseTip;
use base_execution_eip8130::FeeCheck;
use base_execution_txpool::BasePooledTx;
use revm::Database;

/// Minimum intrinsic gas any includable transaction must buy. A gas payer that cannot
/// cover this much gas (at a candidate's fee) cannot fund *any* of its sponsorships, so
/// the shortfall is payer-wide rather than specific to one over-large transaction.
const MIN_INCLUDABLE_GAS: u64 = 21_000;

/// Outcome of the build-time gas-payer affordability check.
///
/// Separates a payer-wide drain (suspend the payer) from a single unaffordable
/// transaction (skip only), so one over-large transaction never revokes a payer's other,
/// cheaper sponsorships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GasAffordability {
    /// Worst-case gas (and any declared coinbase tip) is payable; include the transaction.
    Affordable,
    /// Only this transaction is unaffordable — an over-large gas limit, or a sponsored
    /// sender that cannot cover its own tip. Skip the transaction but keep the payer
    /// active: its other, cheaper sponsorships may still be includable.
    TransactionOnly,
    /// The gas payer cannot cover even a minimum-sized transaction's gas at this fee, so
    /// none of its sponsorships can be included. Treat as a payer-wide drain and suspend
    /// every candidate that payer funds for the remainder of the build.
    PayerDrained {
        /// The drained gas payer.
        payer: Address,
        /// The payer's balance observed in the in-progress block state.
        balance: U256,
    },
}

/// Whether worst-case EIP-8130 gas, and a declared coinbase tip if present, can
/// be paid from current balances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoinbaseTipAffordability;

impl CoinbaseTipAffordability {
    /// EIP-8130 gas payer: explicit `tx.payer`, or the recovered sender for self-pay.
    pub fn gas_payer<T: BasePooledTx>(tx: &T) -> Option<Address> {
        tx.as_eip8130().map(|signed| signed.tx().payer.unwrap_or_else(|| tx.sender()))
    }

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

    /// Classifies a candidate's worst-case gas affordability, distinguishing a payer-wide
    /// drain (suspend the payer) from a single unaffordable transaction (skip only).
    ///
    /// The gas payer covers gas; a sponsored sender covers only its own tip. A shortfall is
    /// payer-wide only when the payer cannot cover even [`MIN_INCLUDABLE_GAS`] at this
    /// candidate's fee — a drained sponsor — as opposed to one over-large transaction or a
    /// sender short on its tip, either of which leaves the payer able to fund cheaper work.
    ///
    /// Non-8130 transactions are always [`GasAffordability::Affordable`] here; the EVM fee
    /// deduction still rejects them if they cannot pay. A failed account read fails open
    /// (treated as affordable) so a transient DB error does not drop a valid candidate.
    pub fn gas_shortfall<T, DB>(tx: &T, payer_auth: u64, db: &mut DB) -> GasAffordability
    where
        T: BasePooledTx,
        DB: Database,
    {
        let Some(signed) = tx.as_eip8130() else {
            return GasAffordability::Affordable;
        };
        let sender = tx.sender();
        let payer = signed.tx().payer.unwrap_or(sender);
        let Ok(payer_info) = db.basic(payer) else {
            return GasAffordability::Affordable;
        };
        let payer_balance = payer_info.map_or(U256::ZERO, |info| info.balance);
        let payer_is_sender = payer == sender;
        let sender_balance = if payer_is_sender {
            payer_balance
        } else {
            let Ok(sender_info) = db.basic(sender) else {
                return GasAffordability::Affordable;
            };
            sender_info.map_or(U256::ZERO, |info| info.balance)
        };
        let tip = CoinbaseTip::decode(signed.tx(), sender).unwrap_or(U256::ZERO);
        let max_fee = tx.max_fee_per_gas();
        if FeeCheck::validate_gas_and_tip(
            payer_balance,
            sender_balance,
            payer_is_sender,
            tx.gas_limit(),
            payer_auth,
            max_fee,
            tip,
        )
        .is_ok()
        {
            return GasAffordability::Affordable;
        }
        // Unaffordable. Payer-wide only when the payer cannot cover even a minimum-sized
        // transaction's gas at this fee: a drained sponsor, not one over-large transaction
        // (nor a sponsored sender short only on its tip, which leaves the payer's balance
        // untouched and therefore above this floor).
        let min_gas = FeeCheck::max_fee_charge(MIN_INCLUDABLE_GAS, payer_auth, max_fee);
        if payer_balance < min_gas {
            GasAffordability::PayerDrained { payer, balance: payer_balance }
        } else {
            GasAffordability::TransactionOnly
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, Eip8130Signed, TxEip8130,
    };
    use base_execution_txpool::BasePooledTransaction;
    use reth_transaction_pool::PoolTransaction;
    use revm::{
        Database,
        database::InMemoryDB,
        database_interface::DBErrorMarker,
        state::{AccountInfo, Bytecode},
    };

    use super::{CoinbaseTipAffordability, GasAffordability};

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

    #[test]
    fn zero_tip_still_rejects_empty_payer() {
        let mut db = InMemoryDB::default();
        assert!(CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            PAYER,
            21_000,
            0,
            2,
            U256::ZERO,
            &mut db
        ));
    }

    #[test]
    fn zero_tip_is_affordable_when_payer_covers_gas() {
        let mut db = InMemoryDB::default();
        fund(&mut db, PAYER, 42_000);
        assert!(!CoinbaseTipAffordability::unaffordable_tip(
            SENDER,
            PAYER,
            21_000,
            0,
            2,
            U256::ZERO,
            &mut db
        ));
    }

    /// Builds an EIP-8130 pooled transaction with no coinbase tip. `max_fee` of `1`
    /// makes gas math read directly: payer gas is `gas_limit`, the payer-wide floor is
    /// [`super::MIN_INCLUDABLE_GAS`]. `payer: None` is self-pay.
    fn eip8130(
        sender: &PrivateKeySigner,
        payer: Option<Address>,
        gas_limit: u64,
        max_fee: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: max_fee,
            gas_limit,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer,
        };
        let sender_auth = Bytes::from(
            sender.sign_hash_sync(&tx.sender_signature_hash()).unwrap().as_bytes().to_vec(),
        );
        let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, sender.address()))
    }

    #[test]
    fn sponsored_gas_within_balance_is_affordable() {
        let sender = PrivateKeySigner::random();
        let payer = PrivateKeySigner::random();
        let tx = eip8130(&sender, Some(payer.address()), 21_000, 1);
        let mut db = InMemoryDB::default();
        fund(&mut db, payer.address(), 100_000);

        assert_eq!(
            CoinbaseTipAffordability::gas_shortfall(&tx, 0, &mut db),
            GasAffordability::Affordable
        );
    }

    #[test]
    fn drained_sponsor_is_a_payer_wide_shortfall() {
        let sender = PrivateKeySigner::random();
        let payer = PrivateKeySigner::random();
        let tx = eip8130(&sender, Some(payer.address()), 21_000, 1);
        // Payer left unfunded: it cannot cover even a minimum-sized transaction's gas.
        let mut db = InMemoryDB::default();

        assert_eq!(
            CoinbaseTipAffordability::gas_shortfall(&tx, 0, &mut db),
            GasAffordability::PayerDrained { payer: payer.address(), balance: U256::ZERO }
        );
    }

    #[test]
    fn one_over_large_transaction_does_not_suspend_a_funded_payer() {
        let sender = PrivateKeySigner::random();
        let payer = PrivateKeySigner::random();
        // Funds a minimum-sized transaction (21_000 gas) but not this 1_000_000-gas one.
        let tx = eip8130(&sender, Some(payer.address()), 1_000_000, 1);
        let mut db = InMemoryDB::default();
        fund(&mut db, payer.address(), 50_000);

        assert_eq!(
            CoinbaseTipAffordability::gas_shortfall(&tx, 0, &mut db),
            GasAffordability::TransactionOnly
        );
    }

    #[test]
    fn drained_self_payer_is_a_payer_wide_shortfall() {
        let sender = PrivateKeySigner::random();
        let tx = eip8130(&sender, None, 21_000, 1);
        let mut db = InMemoryDB::default();

        assert_eq!(
            CoinbaseTipAffordability::gas_shortfall(&tx, 0, &mut db),
            GasAffordability::PayerDrained { payer: sender.address(), balance: U256::ZERO }
        );
    }

    #[test]
    fn gas_shortfall_fails_open_on_database_error() {
        let sender = PrivateKeySigner::random();
        let payer = PrivateKeySigner::random();
        let tx = eip8130(&sender, Some(payer.address()), 21_000, 1);

        assert_eq!(
            CoinbaseTipAffordability::gas_shortfall(&tx, 0, &mut FailingDatabase),
            GasAffordability::Affordable
        );
    }
}
