//! Transaction ordering strategies for the Base mempool.
//!
//! [`UnifiedTipOrdering`] ranks by tip-per-gas — an EIP-1559 priority fee, or a
//! statically decoded EIP-8130 coinbase tip over `gas_limit` — and breaks ties
//! in favor of fewer validity predicates. [`TimestampOrdering`] is FIFO.

use std::{
    cmp::{Ordering, Reverse},
    marker::PhantomData,
    sync::Arc,
    time::Instant,
};

use alloy_primitives::{TxHash, U256};
use base_common_consensus::CoinbaseTip;
use reth_transaction_pool::{PoolTransaction, Priority, TransactionOrdering, ValidPoolTransaction};

use crate::{BasePooledTransaction, BasePooledTx, TimestampedTransaction};

/// Complete priority key used when merging best-transaction sources.
///
/// Higher ordering priority wins. Transactions with the same ordering priority are ordered by
/// arrival time and then transaction hash, matching the existing merged iterator semantics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BestTransactionPriority<P: Ord + Clone> {
    priority: Priority<P>,
    timestamp: Reverse<Instant>,
    hash: TxHash,
}

impl<P: Ord + Clone> BestTransactionPriority<P> {
    /// Computes a complete priority key for a validated pool transaction.
    pub fn new<T, O>(
        ordering: &O,
        transaction: &Arc<ValidPoolTransaction<T>>,
        base_fee: u64,
    ) -> Self
    where
        T: PoolTransaction,
        O: TransactionOrdering<Transaction = T, PriorityValue = P>,
    {
        Self {
            priority: ordering.priority(&transaction.transaction, base_fee),
            timestamp: Reverse(transaction.timestamp),
            hash: *transaction.hash(),
        }
    }
}

/// Transaction ordering strategy for the pool.
///
/// Each variant holds only the ordering implementation it needs.
#[derive(Debug)]
pub enum BaseOrdering<T> {
    /// Order by unified tip-per-gas, with fewer validity predicates winning ties.
    CoinbaseTip(UnifiedTipOrdering<T>),
    /// Order by receive timestamp (FIFO, earlier = higher priority).
    Timestamp(TimestampOrdering<T>),
}

impl<T> BaseOrdering<T> {
    /// Creates unified tip-per-gas ordering (default pool ranking).
    pub fn coinbase_tip() -> Self {
        Self::CoinbaseTip(UnifiedTipOrdering::default())
    }

    /// Creates a new timestamp ordering (FIFO).
    pub fn timestamp() -> Self {
        Self::Timestamp(TimestampOrdering::default())
    }
}

impl<T> Clone for BaseOrdering<T> {
    fn clone(&self) -> Self {
        match self {
            Self::CoinbaseTip(ordering) => Self::CoinbaseTip(ordering.clone()),
            Self::Timestamp(ordering) => Self::Timestamp(ordering.clone()),
        }
    }
}

impl<T> Default for BaseOrdering<T> {
    fn default() -> Self {
        Self::coinbase_tip()
    }
}

/// Tip-per-gas bid with a fewer-predicates tiebreak.
///
/// The bid is the rational `tip / gas`. Standard transactions and EIP-8130
/// transactions without a static coinbase tip use `effective_tip_per_gas` as
/// `tip` and `gas = 1`. A statically decoded coinbase tip uses that amount as
/// `tip` and the transaction `gas_limit` as `gas`.
///
/// Equality and ordering compare the bid by cross-multiplication, then fewer
/// predicates. Distinct `(tip, gas)` pairs that represent the same ratio compare
/// equal.
#[derive(Debug, Clone, Copy)]
pub struct UnifiedTipPriority {
    /// Numerator of the tip-per-gas bid.
    pub tip: U256,
    /// Denominator of the tip-per-gas bid (`gas_limit`, or `1` for a priority-fee bid).
    pub gas: u64,
    /// Fewer predicates rank higher (`Reverse` so `0` beats `1`).
    pub predicates: Reverse<u32>,
}

impl UnifiedTipPriority {
    /// Builds a priority from a tip amount, gas denominator, and predicate count.
    pub fn new(tip: U256, gas: u64, predicate_count: usize) -> Self {
        Self {
            tip,
            gas: gas.max(1),
            predicates: Reverse(u32::try_from(predicate_count).unwrap_or(u32::MAX)),
        }
    }
}

impl Default for UnifiedTipPriority {
    fn default() -> Self {
        Self::new(U256::ZERO, 1, 0)
    }
}

impl PartialEq for UnifiedTipPriority {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for UnifiedTipPriority {}

impl PartialOrd for UnifiedTipPriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for UnifiedTipPriority {
    fn cmp(&self, other: &Self) -> Ordering {
        // `tip * gas` fits in U256: a larger product would already overflow the
        // transaction's total fee spend.
        let left = self.tip * U256::from(other.gas);
        let right = other.tip * U256::from(self.gas);
        left.cmp(&right).then_with(|| self.predicates.cmp(&other.predicates))
    }
}

/// Unified tip-per-gas ordering for standard and EIP-8130 transactions.
///
/// Uses [`CoinbaseTip::decode`] when the transaction is a statically-analyzable
/// EIP-8130 coinbase tip; otherwise ranks by `effective_tip_per_gas`. Same bid
/// prefers fewer validity predicates, so standard transactions beat equally
/// priced advanced transactions.
#[derive(Debug)]
#[non_exhaustive]
pub struct UnifiedTipOrdering<T = BasePooledTransaction>(PhantomData<T>);

impl<T> Default for UnifiedTipOrdering<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T> Clone for UnifiedTipOrdering<T> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl<T> TransactionOrdering for UnifiedTipOrdering<T>
where
    T: BasePooledTx + 'static,
{
    type PriorityValue = UnifiedTipPriority;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        let predicates = transaction.validity_predicates().len();
        if let Some(signed) = transaction.as_eip8130()
            && let Some(tip) = CoinbaseTip::decode(signed.tx(), transaction.sender())
        {
            return Priority::Value(UnifiedTipPriority::new(
                tip,
                transaction.gas_limit(),
                predicates,
            ));
        }
        transaction.effective_tip_per_gas(base_fee).map_or(Priority::None, |tip| {
            Priority::Value(UnifiedTipPriority::new(U256::from(tip), 1, predicates))
        })
    }
}

/// Pool priority value for [`BaseOrdering`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BasePriority {
    /// Unified tip-per-gas bid plus predicate-count tiebreak.
    Unified(UnifiedTipPriority),
    /// Inverted receive timestamp (`u128::MAX - received_at`).
    Timestamp(u128),
}

impl Default for BasePriority {
    fn default() -> Self {
        Self::Unified(UnifiedTipPriority::default())
    }
}

impl PartialOrd for BasePriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BasePriority {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Unified(left), Self::Unified(right)) => left.cmp(right),
            (Self::Timestamp(left), Self::Timestamp(right)) => left.cmp(right),
            (Self::Unified(_), Self::Timestamp(_)) => Ordering::Less,
            (Self::Timestamp(_), Self::Unified(_)) => Ordering::Greater,
        }
    }
}

/// Ordering for transactions based on their timestamp (FIFO).
///
/// Transactions that arrived earlier get higher priority.
/// Uses a timestamp assigned at insertion time for deterministic ordering.
#[derive(Debug)]
#[non_exhaustive]
pub struct TimestampOrdering<T = BasePooledTransaction>(PhantomData<T>);

impl<T> Default for TimestampOrdering<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T> Clone for TimestampOrdering<T> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl<T> TransactionOrdering for TimestampOrdering<T>
where
    T: PoolTransaction + TimestampedTransaction + 'static,
{
    type PriorityValue = u128;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        _base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        // Reth sorts descending (higher value = picked first).
        // We want older transactions (lower timestamp) first,
        // so invert: MAX - timestamp.
        Priority::Value(u128::MAX - transaction.received_at())
    }
}

impl<T> TransactionOrdering for BaseOrdering<T>
where
    T: BasePooledTx + TimestampedTransaction + 'static,
{
    type PriorityValue = BasePriority;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        match self {
            Self::CoinbaseTip(ordering) => match ordering.priority(transaction, base_fee) {
                Priority::Value(priority) => Priority::Value(BasePriority::Unified(priority)),
                Priority::None => Priority::None,
            },
            Self::Timestamp(ordering) => match ordering.priority(transaction, base_fee) {
                Priority::Value(priority) => Priority::Value(BasePriority::Timestamp(priority)),
                Priority::None => Priority::None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use alloy_sol_types::SolCall;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, BaseTransactionSigned, Call,
        Eip8130Signed, IDefaultAccount, Predeploys, TxEip8130,
    };
    use base_test_utils::Account;
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::{
        CoinbaseTipOrdering, PoolTransaction, TransactionOrdering, test_utils::TransactionBuilder,
    };

    use super::*;
    use crate::{BasePooledTransaction, ValidityOperator, ValidityPredicate};

    fn create_test_tx(nonce: u64) -> BasePooledTransaction {
        let alice = Account::Alice;
        let bob = Account::Bob;

        let signed_tx = TransactionBuilder::default()
            .signer(alice.signer_b256())
            .chain_id(1)
            .nonce(nonce)
            .to(bob.address())
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();
        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let recovered = Recovered::new_unchecked(tx, alice.address());
        let len = recovered.encode_2718_len();
        BasePooledTransaction::new(recovered, len)
    }

    fn create_test_tx_with_timestamp(nonce: u64, received_at: u128) -> BasePooledTransaction {
        let alice = Account::Alice;
        let bob = Account::Bob;

        let signed_tx = TransactionBuilder::default()
            .signer(alice.signer_b256())
            .chain_id(1)
            .nonce(nonce)
            .to(bob.address())
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();
        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let recovered = Recovered::new_unchecked(tx, alice.address());
        let len = recovered.encode_2718_len();
        BasePooledTransaction::new_with_received_at(recovered, len, received_at)
    }

    fn create_test_tx_from(
        account: Account,
        nonce: u64,
        received_at: u128,
        max_priority_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let bob = Account::Bob;

        let signed_tx = TransactionBuilder::default()
            .signer(account.signer_b256())
            .chain_id(1)
            .nonce(nonce)
            .to(bob.address())
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(max_priority_fee_per_gas)
            .into_eip1559();
        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let recovered = Recovered::new_unchecked(tx, account.address());
        let len = recovered.encode_2718_len();
        BasePooledTransaction::new_with_received_at(recovered, len, received_at)
    }

    fn eip1559_pooled(
        nonce: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        gas_limit: u64,
    ) -> BasePooledTransaction {
        let alice = Account::Alice;
        let bob = Account::Bob;
        let signed_tx = TransactionBuilder::default()
            .signer(alice.signer_b256())
            .chain_id(1)
            .nonce(nonce)
            .to(bob.address())
            .value(1_000)
            .gas_limit(gas_limit)
            .max_fee_per_gas(max_fee_per_gas)
            .max_priority_fee_per_gas(max_priority_fee_per_gas)
            .into_eip1559();
        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let recovered = Recovered::new_unchecked(tx, alice.address());
        let len = recovered.encode_2718_len();
        BasePooledTransaction::new(recovered, len)
    }

    fn encode_execute(target: Address, value: U256) -> Bytes {
        Bytes::from(
            IDefaultAccount::executeCall { target, value, data: Default::default() }.abi_encode(),
        )
    }

    fn eip8130_pooled(
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        gas_limit: u64,
        coinbase_tip: Option<U256>,
    ) -> BasePooledTransaction {
        let signer = PrivateKeySigner::random();
        let calls = coinbase_tip.map_or_else(Vec::new, |amount| {
            vec![vec![Call {
                to: signer.address(),
                data: encode_execute(Predeploys::SEQUENCER_FEE_VAULT, amount),
            }]]
        });
        let tx = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit,
            account_changes: Vec::new(),
            calls,
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn block_number_predicate() -> ValidityPredicate {
        ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(1),
        }
    }

    #[test]
    fn test_older_tx_has_higher_priority() {
        let ordering = TimestampOrdering::<BasePooledTransaction>::default();

        let older_tx = create_test_tx_with_timestamp(1, 1000);
        let newer_tx = create_test_tx_with_timestamp(2, 2000);

        let older_priority = ordering.priority(&older_tx, 0);
        let newer_priority = ordering.priority(&newer_tx, 0);
        assert!(
            older_priority > newer_priority,
            "older_tx priority should be greater than newer_tx priority",
        );
    }

    #[test]
    fn test_priority_value_is_max_minus_timestamp() {
        let ordering = TimestampOrdering::<BasePooledTransaction>::default();
        let tx = create_test_tx(1);

        let priority = ordering.priority(&tx, 0);
        match priority {
            Priority::Value(val) => {
                assert_eq!(val, u128::MAX - tx.received_at());
            }
            Priority::None => panic!("Expected Priority::Value"),
        }
    }

    #[test]
    fn test_base_ordering_coinbase_tip_mode() {
        let ordering = BaseOrdering::<BasePooledTransaction>::coinbase_tip();

        let higher_tip = create_test_tx(1);
        let lower_tip = {
            let alice = Account::Alice;
            let bob = Account::Bob;

            let signed_tx = TransactionBuilder::default()
                .signer(alice.signer_b256())
                .chain_id(1)
                .nonce(2)
                .to(bob.address())
                .value(1_000)
                .gas_limit(21_000)
                .max_fee_per_gas(10)
                .max_priority_fee_per_gas(0)
                .into_eip1559();
            let tx = BaseTransactionSigned::Eip1559(
                signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
            );
            let recovered = Recovered::new_unchecked(tx, alice.address());
            let len = recovered.encode_2718_len();
            BasePooledTransaction::new(recovered, len)
        };

        let higher_priority = ordering.priority(&higher_tip, 0);
        let lower_priority = ordering.priority(&lower_tip, 0);
        assert!(higher_priority > lower_priority);
    }

    #[test]
    fn test_base_ordering_timestamp_mode() {
        let ordering = BaseOrdering::<BasePooledTransaction>::timestamp();

        let older_tx = create_test_tx_with_timestamp(1, 1000);
        let newer_tx = create_test_tx_with_timestamp(2, 2000);

        let older_priority = ordering.priority(&older_tx, 0);
        let newer_priority = ordering.priority(&newer_tx, 0);
        assert!(older_priority > newer_priority);
    }

    #[test]
    fn test_base_ordering_default_is_unified_tip() {
        let ordering = BaseOrdering::<BasePooledTransaction>::default();
        let unified = UnifiedTipOrdering::<BasePooledTransaction>::default();
        let tx = create_test_tx(1);
        match (ordering.priority(&tx, 0), unified.priority(&tx, 0)) {
            (Priority::Value(BasePriority::Unified(base)), Priority::Value(inner)) => {
                assert_eq!(base, inner);
            }
            other => panic!("expected matching unified priorities, got {other:?}"),
        }
    }

    // NOTE: Same-sender nonce ordering is enforced by the txpool layer, not the
    // ordering trait. The `TransactionOrdering::priority` method is called per
    // transaction in isolation — it has no visibility into sender or nonce.
    // Reth's txpool first groups pending transactions by sender and orders them
    // by nonce within each sender's queue, then uses the ordering's priority to
    // rank across senders. A full integration test against the txpool is needed
    // to verify same-sender nonce ordering interacts correctly with timestamp
    // priority.
    //
    // This test verifies the ordering's behavior for transactions from the same
    // sender: the one with the lower timestamp should receive higher priority,
    // regardless of nonce.
    #[test]
    fn test_same_sender_timestamp_ordering() {
        let ordering = BaseOrdering::<BasePooledTransaction>::timestamp();

        let tx_nonce_0 = create_test_tx_from(Account::Alice, 0, 1000, 1);
        let tx_nonce_1 = create_test_tx_from(Account::Alice, 1, 2000, 1);

        let priority_0 = ordering.priority(&tx_nonce_0, 0);
        let priority_1 = ordering.priority(&tx_nonce_1, 0);

        assert!(
            priority_0 > priority_1,
            "earlier tx should have higher priority regardless of nonce",
        );
    }

    #[test]
    fn standard_tx_ranking_matches_reth_coinbase_tip_order() {
        let unified = UnifiedTipOrdering::<BasePooledTransaction>::default();
        let reth = CoinbaseTipOrdering::<BasePooledTransaction>::default();
        let higher = eip1559_pooled(1, 10, 2, 21_000);
        let lower = eip1559_pooled(2, 10, 1, 21_000);

        assert_eq!(
            unified.priority(&higher, 0).cmp(&unified.priority(&lower, 0)),
            reth.priority(&higher, 0).cmp(&reth.priority(&lower, 0)),
        );
        assert!(unified.priority(&higher, 0) > unified.priority(&lower, 0));
    }

    #[test]
    fn coinbase_tip_per_gas_competes_with_priority_fee() {
        let ordering = UnifiedTipOrdering::<BasePooledTransaction>::default();
        let standard = eip1559_pooled(1, 10, 2, 21_000);
        let cheaper_at = eip8130_pooled(10, 0, 21_000, Some(U256::from(21_000)));
        let richer_at = eip8130_pooled(10, 0, 21_000, Some(U256::from(63_000)));

        assert!(ordering.priority(&standard, 0) > ordering.priority(&cheaper_at, 0));
        assert!(ordering.priority(&richer_at, 0) > ordering.priority(&standard, 0));
    }

    #[test]
    fn fewer_predicates_win_equal_bid() {
        let ordering = UnifiedTipOrdering::<BasePooledTransaction>::default();
        let standard = eip1559_pooled(1, 10, 2, 21_000);
        let one_predicate = eip8130_pooled(10, 0, 21_000, Some(U256::from(42_000)))
            .with_validity_predicates(vec![block_number_predicate()]);
        let two_predicates = eip8130_pooled(10, 0, 21_000, Some(U256::from(42_000)))
            .with_validity_predicates(vec![block_number_predicate(), block_number_predicate()]);

        assert!(ordering.priority(&standard, 0) > ordering.priority(&one_predicate, 0));
        assert!(ordering.priority(&one_predicate, 0) > ordering.priority(&two_predicates, 0));
    }

    #[test]
    fn eip8130_without_static_tip_uses_priority_fee() {
        let ordering = UnifiedTipOrdering::<BasePooledTransaction>::default();
        let standard = eip1559_pooled(1, 10, 3, 21_000);
        let aa = eip8130_pooled(10, 3, 50_000, None);

        assert_eq!(ordering.priority(&standard, 0), ordering.priority(&aa, 0));
    }

    #[test]
    fn equivalent_ratios_compare_equal_before_predicate_tiebreak() {
        assert_eq!(
            UnifiedTipPriority::new(U256::from(3u64), 2, 0),
            UnifiedTipPriority::new(U256::from(6u64), 4, 0),
        );
        assert!(
            UnifiedTipPriority::new(U256::from(3u64), 2, 0)
                > UnifiedTipPriority::new(U256::from(1u64), 1, 0)
        );
        assert!(
            UnifiedTipPriority::new(U256::from(3u64), 2, 0)
                > UnifiedTipPriority::new(U256::from(3u64), 2, 1)
        );
    }

    #[test]
    fn below_base_fee_standard_tx_has_no_priority() {
        let ordering = UnifiedTipOrdering::<BasePooledTransaction>::default();
        let tx = eip1559_pooled(1, 10, 1, 21_000);
        assert_eq!(ordering.priority(&tx, 20), Priority::None);
    }
}
