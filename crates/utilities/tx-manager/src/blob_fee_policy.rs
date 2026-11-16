//! Fee policy of blob (EIP-4844) transactions.

use std::sync::{Arc, Mutex};

use alloy_eips::{eip4844::DATA_GAS_PER_BLOB, eip7840::BlobParams};
use alloy_rpc_types_eth::{Header, TransactionReceipt};

use crate::{FeeCalculator, FeeOverride, PreparedTx, TxManagerError, TxManagerResult};

/// Prices blob transactions from `p`, a `max_priority_fee_per_gas` per blob carried from
/// one blob transaction to the next, instead of the priority fee the L1 node suggests.
///
/// `p` starts at the lower bound. A new transaction with `n` blobs gets a
/// `max_priority_fee_per_gas` of `p × n`, a `max_fee_per_gas` of that plus twice the base
/// fee, and a `max_fee_per_blob_gas` of twice the blob base fee ([`fees`](Self::fees)).
///
/// A transaction still pending after the resubmission timeout is replaced only when an L1
/// block mined meanwhile priced it out: the block could not include it at its fee caps
/// ([`is_affordable`](Self::is_affordable)), or filled its blob space with blob transactions
/// that paid builders more per blob ([`is_outbid`](Self::is_outbid)). A block without blobs, or with
/// room left for it, says nothing a higher fee would fix.
///
/// A replacement doubles all three fields, the minimum L1 nodes accept to replace a blob
/// transaction ([`replacement`](Self::replacement)), and is signed with these as a floor
/// over the fees of a new transaction ([`fees`](Self::fees)). So its fee caps follow L1
/// prices when they rose further, and its `max_priority_fee_per_gas` is `p × n` when
/// another transaction raised `p` above twice its previous value. Once published, it
/// raises `p` to its `max_priority_fee_per_gas` per blob
/// ([`record_replacement`](Self::record_replacement)).
/// When a transaction lands, `p` becomes 90% of the `max_priority_fee_per_gas` per blob it
/// paid, and never less than the lower bound ([`record_landed`](Self::record_landed)).
///
/// No `max_priority_fee_per_gas` per blob goes above the upper bound: a fee override above
/// `upper_bound × n` is refused. A replacement that cannot double its
/// `max_priority_fee_per_gas` within `upper_bound × n` would be refused by L1 nodes, so a
/// transaction that reaches that point is not replaced any more and stays at its last fees.
///
/// Clones share `p`, so every send of a manager and its clones carries the same value. It
/// lives in memory only and starts again from the lower bound after a restart.
#[derive(Debug, Clone)]
pub struct BlobFeePolicy {
    /// Lowest `max_priority_fee_per_gas` per blob, in wei.
    lower_bound: u128,
    /// Highest `max_priority_fee_per_gas` per blob, in wei.
    upper_bound: u128,
    /// `p`, the `max_priority_fee_per_gas` per blob of the next blob transaction, in wei.
    max_priority_fee_per_gas_per_blob: Arc<Mutex<u128>>,
}

impl BlobFeePolicy {
    /// Share, in percent, of the `max_priority_fee_per_gas` per blob of a landed transaction
    /// that the next blob transaction starts from.
    pub const MAX_PRIORITY_FEE_PER_GAS_DECAY_PERCENT: u128 = 90;

    /// Creates a policy whose `max_priority_fee_per_gas` per blob starts at `lower_bound` and
    /// stays between `lower_bound` and `upper_bound`, in wei.
    pub fn new(lower_bound: u128, upper_bound: u128) -> Self {
        Self {
            lower_bound,
            upper_bound,
            max_priority_fee_per_gas_per_blob: Arc::new(Mutex::new(lower_bound)),
        }
    }

    /// Returns `p`, the `max_priority_fee_per_gas` per blob of the next blob transaction.
    pub fn max_priority_fee_per_gas_per_blob(&self) -> u128 {
        *self.max_priority_fee_per_gas_per_blob.lock().expect("BlobFeePolicy mutex poisoned")
    }

    /// Returns the fees of a transaction with `blob_count` blobs at the current L1
    /// `base_fee` and `blob_base_fee`, no lower than `floor`, such as the fees of a
    /// [`replacement`](Self::replacement). A new transaction passes no floor
    /// ([`FeeOverride::default`]).
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::FeeLimitExceeded`] when `floor` raises the
    /// `max_priority_fee_per_gas` above `upper_bound × blob_count`.
    pub fn fees(
        &self,
        blob_count: u128,
        base_fee: u128,
        blob_base_fee: u128,
        floor: FeeOverride,
    ) -> TxManagerResult<FeeOverride> {
        let max_priority_fee_per_gas = self
            .max_priority_fee_per_gas_per_blob()
            .saturating_mul(blob_count)
            .max(floor.gas_tip_cap);
        self.check_upper_bound(max_priority_fee_per_gas, blob_count)?;
        let max_fee_per_gas = FeeCalculator::calc_gas_fee_cap(base_fee, max_priority_fee_per_gas)
            .max(floor.gas_fee_cap);
        let max_fee_per_blob_gas = FeeCalculator::calc_blob_fee_cap(blob_base_fee)
            .max(floor.blob_fee_cap.unwrap_or_default());
        Ok(FeeOverride::new(max_priority_fee_per_gas, max_fee_per_gas)
            .with_blob_fee_cap(max_fee_per_blob_gas))
    }

    /// Returns the lowest fees that replace `old`, a transaction with `blob_count` blobs:
    /// twice each of its fees. [`fees`](Self::fees) raises them to `p` and the current L1
    /// prices.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::FeeLimitExceeded`] when the doubled
    /// `max_priority_fee_per_gas` would pass `upper_bound × blob_count`: no replacement
    /// within the bound would be accepted by L1 nodes.
    pub fn replacement(&self, old: &PreparedTx, blob_count: u128) -> TxManagerResult<FeeOverride> {
        let max_priority_fee_per_gas = FeeCalculator::calc_threshold_value(old.gas_tip_cap, true);
        self.check_upper_bound(max_priority_fee_per_gas, blob_count)?;
        Ok(FeeOverride::new(
            max_priority_fee_per_gas,
            FeeCalculator::calc_threshold_value(old.gas_fee_cap, true),
        )
        .with_blob_fee_cap(FeeCalculator::calc_threshold_value(
            old.blob_fee_cap.unwrap_or_default(),
            true,
        )))
    }

    /// Checks that `max_priority_fee_per_gas`, of a transaction with `blob_count` blobs,
    /// is within `upper_bound × blob_count`.
    const fn check_upper_bound(
        &self,
        max_priority_fee_per_gas: u128,
        blob_count: u128,
    ) -> TxManagerResult<()> {
        let ceiling = self.upper_bound.saturating_mul(blob_count);
        if max_priority_fee_per_gas > ceiling {
            return Err(TxManagerError::FeeLimitExceeded {
                fee: max_priority_fee_per_gas,
                ceiling,
            });
        }
        Ok(())
    }

    /// Returns whether a block with `header` could include `tx`, a blob transaction, at its
    /// fee caps: the block's base fee is within its `max_fee_per_gas`, and its blob base fee,
    /// at `blob_params`, within its `max_fee_per_blob_gas`.
    pub fn is_affordable(tx: &PreparedTx, header: &Header, blob_params: BlobParams) -> bool {
        u128::from(header.base_fee_per_gas.unwrap_or_default()) <= tx.gas_fee_cap
            && header
                .inner
                .blob_fee(blob_params)
                .is_none_or(|blob_base_fee| blob_base_fee <= tx.blob_fee_cap.unwrap_or_default())
    }

    /// Returns whether a block with `base_fee` and `receipts` left no room for `tx`, a blob
    /// transaction with `blob_count` blobs: its blob transactions that paid builders more
    /// per blob than `tx` would hold more than `max_blob_count` minus `blob_count` blobs.
    ///
    /// A builder earns a transaction's tip times the gas it uses, so bids are compared as
    /// `tip × gas used / blobs`. A mined transaction's tip is its effective gas price above
    /// `base_fee`. `tx` bids the tip it would pay at `base_fee` over its gas limit, which is
    /// the gas a blob transaction to an account uses.
    pub fn is_outbid(
        tx: &PreparedTx,
        blob_count: u128,
        base_fee: u64,
        receipts: &[TransactionReceipt],
        max_blob_count: u64,
    ) -> bool {
        let tip = tx.gas_tip_cap.min(tx.gas_fee_cap.saturating_sub(u128::from(base_fee)));
        let bid = tip.saturating_mul(u128::from(tx.gas_limit));
        let outbidding_blobs: u128 = receipts
            .iter()
            .filter_map(|other| {
                let other_blobs = u128::from(other.blob_gas_used? / DATA_GAS_PER_BLOB);
                let other_bid = other
                    .effective_gas_price
                    .saturating_sub(u128::from(base_fee))
                    .saturating_mul(u128::from(other.gas_used));
                (other_bid.saturating_mul(blob_count) > bid.saturating_mul(other_blobs))
                    .then_some(other_blobs)
            })
            .sum();
        outbidding_blobs + blob_count > u128::from(max_blob_count)
    }

    /// Records that a replacement with `blob_count` blobs and `max_priority_fee_per_gas` was
    /// published: the next blob transactions start at least at its
    /// `max_priority_fee_per_gas` per blob.
    pub fn record_replacement(&self, max_priority_fee_per_gas: u128, blob_count: u128) {
        let mut p =
            self.max_priority_fee_per_gas_per_blob.lock().expect("BlobFeePolicy mutex poisoned");
        *p = (*p).max(max_priority_fee_per_gas / blob_count);
    }

    /// Records that a transaction with `blob_count` blobs landed with
    /// `max_priority_fee_per_gas`: the next blob transaction starts from 90% of its
    /// `max_priority_fee_per_gas` per blob, and never below the lower bound.
    pub fn record_landed(&self, max_priority_fee_per_gas: u128, blob_count: u128) {
        let decayed = max_priority_fee_per_gas
            .saturating_mul(Self::MAX_PRIORITY_FEE_PER_GAS_DECAY_PERCENT)
            / 100
            / blob_count;
        *self.max_priority_fee_per_gas_per_blob.lock().expect("BlobFeePolicy mutex poisoned") =
            decayed.max(self.lower_bound);
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, B256, Bytes};
    use rstest::rstest;

    use super::*;

    const GWEI: u128 = 1_000_000_000;

    /// 0.01 to 10 gwei per blob, the batcher's defaults.
    fn policy() -> BlobFeePolicy {
        BlobFeePolicy::new(GWEI / 100, 10 * GWEI)
    }

    /// A signed blob transaction with the given fees, in wei.
    fn sent(
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
        max_fee_per_blob_gas: u128,
    ) -> PreparedTx {
        PreparedTx {
            raw_tx: Bytes::new(),
            gas_tip_cap: max_priority_fee_per_gas,
            gas_fee_cap: max_fee_per_gas,
            blob_fee_cap: Some(max_fee_per_blob_gas),
            gas_limit: 21_000,
            nonce: 0,
            sidecar: None,
        }
    }

    /// The `(max_priority_fee_per_gas, max_fee_per_gas, max_fee_per_blob_gas)` of `fees`.
    const fn fields(fees: &FeeOverride) -> (u128, u128, Option<u128>) {
        (fees.gas_tip_cap, fees.gas_fee_cap, fees.blob_fee_cap)
    }

    /// The receipt of a transaction with `blob_count` blobs, mined in a block with a 1 gwei
    /// base fee, that paid a tip of `tip` wei per gas over `gas_used` gas.
    fn mined(tip: u128, gas_used: u64, blob_count: u64) -> TransactionReceipt {
        TransactionReceipt {
            inner: ReceiptEnvelope::Eip4844(ReceiptWithBloom::default()),
            transaction_hash: B256::ZERO,
            transaction_index: None,
            block_hash: None,
            block_number: None,
            gas_used,
            effective_gas_price: GWEI + tip,
            blob_gas_used: (blob_count > 0).then_some(blob_count * DATA_GAS_PER_BLOB),
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        }
    }

    #[rstest]
    #[case::one_blob(1)]
    #[case::six_blobs(6)]
    fn a_new_transaction_starts_at_the_lower_bound_per_blob(#[case] blob_count: u128) {
        let fees = policy().fees(blob_count, 3 * GWEI, 5 * GWEI, FeeOverride::default()).unwrap();

        assert_eq!(
            fields(&fees),
            (blob_count * GWEI / 100, blob_count * GWEI / 100 + 6 * GWEI, Some(10 * GWEI))
        );
    }

    /// A replacement doubles all three fields, or takes twice the current L1 prices when they
    /// rose further.
    #[rstest]
    #[case::doubles_when_prices_are_flat(GWEI, GWEI, (12, 32, 40))]
    #[case::follows_a_base_fee_rise(30 * GWEI, GWEI, (12, 72, 40))]
    #[case::follows_a_blob_base_fee_rise(GWEI, 30 * GWEI, (12, 32, 60))]
    fn a_replacement_doubles_the_fees_or_follows_l1_prices(
        #[case] base_fee: u128,
        #[case] blob_base_fee: u128,
        #[case] expected_gwei: (u128, u128, u128),
    ) {
        let old = sent(6 * GWEI, 16 * GWEI, 20 * GWEI);

        let policy = policy();
        let fees =
            policy.fees(6, base_fee, blob_base_fee, policy.replacement(&old, 6).unwrap()).unwrap();

        assert_eq!(
            fields(&fees),
            (expected_gwei.0 * GWEI, expected_gwei.1 * GWEI, Some(expected_gwei.2 * GWEI))
        );
    }

    /// A fee override is a floor up to the upper bound, and is refused above it.
    #[rstest]
    #[case::reaches_the_bound(60 * GWEI, Ok(60 * GWEI))]
    #[case::passes_the_bound(60 * GWEI + 1, Err(TxManagerError::FeeLimitExceeded { fee: 60 * GWEI + 1, ceiling: 60 * GWEI }))]
    fn a_fee_override_stays_within_the_upper_bound(
        #[case] max_priority_fee_per_gas: u128,
        #[case] expected: TxManagerResult<u128>,
    ) {
        let fees = policy().fees(6, GWEI, GWEI, FeeOverride::new(max_priority_fee_per_gas, 0));

        assert_eq!(fees.map(|fees| fees.gas_tip_cap), expected);
    }

    /// A replacement takes `p × n` when another transaction raised `p` above twice its
    /// previous `max_priority_fee_per_gas` per blob.
    #[test]
    fn a_replacement_follows_a_raised_carried_fee() {
        let old = sent(6 * GWEI, 16 * GWEI, 20 * GWEI);
        let policy = policy();
        policy.record_replacement(4 * GWEI, 1);

        let fees = policy.fees(6, GWEI, GWEI, policy.replacement(&old, 6).unwrap()).unwrap();

        assert_eq!(fields(&fees), (24 * GWEI, 32 * GWEI, Some(40 * GWEI)));
    }

    /// A replacement up to the upper bound is allowed. Past it, no replacement exists.
    #[rstest]
    #[case::reaches_the_bound(30 * GWEI, Ok(60 * GWEI))]
    #[case::would_pass_the_bound(31 * GWEI, Err(TxManagerError::FeeLimitExceeded { fee: 62 * GWEI, ceiling: 60 * GWEI }))]
    #[case::already_at_the_bound(60 * GWEI, Err(TxManagerError::FeeLimitExceeded { fee: 120 * GWEI, ceiling: 60 * GWEI }))]
    fn a_replacement_stays_within_the_upper_bound(
        #[case] max_priority_fee_per_gas: u128,
        #[case] expected: TxManagerResult<u128>,
    ) {
        let old = sent(max_priority_fee_per_gas, max_priority_fee_per_gas + GWEI, GWEI);

        let fees = policy().replacement(&old, 6);

        assert_eq!(fees.map(|fees| fees.gas_tip_cap), expected);
    }

    /// Computing a replacement leaves `p` alone. Publishing it raises `p` to its
    /// `max_priority_fee_per_gas` per blob, whatever the blob count of the next transactions,
    /// and a replacement below `p` never lowers it.
    #[test]
    fn a_published_replacement_raises_the_next_transactions() {
        let policy = policy();
        let replacement = policy.replacement(&sent(6 * GWEI / 100, GWEI, GWEI), 6).unwrap();
        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), GWEI / 100);

        policy.record_replacement(replacement.gas_tip_cap, 6);
        policy.record_replacement(GWEI / 100, 1);

        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), 2 * GWEI / 100);
        assert_eq!(
            policy.fees(1, GWEI, GWEI, FeeOverride::default()).unwrap().gas_tip_cap,
            2 * GWEI / 100
        );
        assert_eq!(
            policy.fees(6, GWEI, GWEI, FeeOverride::default()).unwrap().gas_tip_cap,
            replacement.gas_tip_cap
        );
    }

    /// A landed transaction sets `p` to 90% of what it paid per blob, never below the lower
    /// bound, even when that lowers `p`.
    #[rstest]
    #[case::decays_by_ten_percent(60 * GWEI, 9 * GWEI)]
    #[case::stops_at_the_lower_bound(6 * GWEI / 100, GWEI / 100)]
    fn a_landed_transaction_decays_the_next_ones(
        #[case] max_priority_fee_per_gas: u128,
        #[case] expected_per_blob: u128,
    ) {
        let policy = policy();
        policy.record_replacement(10 * GWEI, 1);

        policy.record_landed(max_priority_fee_per_gas, 6);

        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), expected_per_blob);
    }

    /// In a congestion the doubling catches up with L1 within a few replacements, and calm
    /// blocks bring `p` back down 10% per landed transaction.
    #[test]
    fn a_congestion_climb_then_decay() {
        let policy = policy();
        let mut tx = sent(0, 0, 0);
        let fees = policy.fees(6, GWEI, GWEI, FeeOverride::default()).unwrap();
        tx.gas_tip_cap = fees.gas_tip_cap;
        tx.gas_fee_cap = fees.gas_fee_cap;
        tx.blob_fee_cap = fees.blob_fee_cap;
        for _ in 0..9 {
            let fees = policy.fees(6, GWEI, GWEI, policy.replacement(&tx, 6).unwrap()).unwrap();
            policy.record_replacement(fees.gas_tip_cap, 6);
            tx.gas_tip_cap = fees.gas_tip_cap;
            tx.gas_fee_cap = fees.gas_fee_cap;
            tx.blob_fee_cap = fees.blob_fee_cap;
        }
        // 0.01 gwei per blob doubled 9 times, and one more doubling would pass 10 gwei.
        assert_eq!(tx.gas_tip_cap, 6 * 512 * GWEI / 100);
        assert!(policy.replacement(&tx, 6).is_err());

        policy.record_landed(tx.gas_tip_cap, 6);
        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), 9 * 512 * GWEI / 1_000);
        policy.record_landed(
            policy.fees(6, GWEI, GWEI, FeeOverride::default()).unwrap().gas_tip_cap,
            6,
        );
        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), 81 * 512 * GWEI / 10_000);
    }

    /// A block can include a blob tx while its base fee is within the tx's `max_fee_per_gas`
    /// and its blob base fee within the tx's `max_fee_per_blob_gas`.
    #[rstest]
    #[case::within_both_caps(3 * GWEI, 0, true)]
    #[case::base_fee_above_the_cap(3 * GWEI + 1, 0, false)]
    #[case::blob_base_fee_above_the_cap(3 * GWEI, 100_000_000, false)]
    fn a_block_affords_a_tx_within_its_fee_caps(
        #[case] base_fee: u128,
        #[case] excess_blob_gas: u64,
        #[case] expected: bool,
    ) {
        // The blob base fee is the 1 wei minimum without excess blob gas.
        let tx = sent(GWEI, 3 * GWEI, 1);
        let header = Header {
            inner: alloy_consensus::Header {
                base_fee_per_gas: Some(base_fee as u64),
                excess_blob_gas: Some(excess_blob_gas),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(BlobFeePolicy::is_affordable(&tx, &header, BlobParams::bpo2()), expected);
    }

    /// A 6-blob tx with 21,000 gas paying 0.6 gwei per gas, 2,100 gwei per blob, in a block
    /// with a 1 gwei base fee and 21 blobs at most, is outbid only when blob txs that paid
    /// more per blob, as tip times gas used, leave no room for it.
    #[rstest]
    #[case::room_left(vec![mined(GWEI * 12 / 10, 21_000, 6), mined(GWEI * 18 / 10, 21_000, 9)], false)]
    #[case::filled_by_higher_payers(vec![mined(GWEI * 12 / 10, 21_000, 6), mined(GWEI * 2, 21_000, 10)], true)]
    #[case::cheaper_txs_leave_room(vec![mined(3 * GWEI, 21_000, 15), mined(GWEI / 100, 21_000, 6)], false)]
    #[case::equal_per_blob_leaves_room(vec![mined(GWEI * 6 / 10, 21_000, 6), mined(GWEI, 21_000, 10)], false)]
    #[case::compared_per_blob(vec![mined(GWEI / 5, 21_000, 1); 16], true)]
    #[case::compared_over_gas_used(vec![mined(GWEI / 100, 300_000, 1); 16], true)]
    #[case::txs_without_blobs_hold_none(vec![mined(3 * GWEI, 21_000, 15), mined(10 * GWEI, 21_000, 0)], false)]
    fn a_block_outbids_a_tx_only_when_higher_payers_fill_it(
        #[case] receipts: Vec<TransactionReceipt>,
        #[case] expected: bool,
    ) {
        let tx = sent(6 * GWEI / 10, 3 * GWEI, 1);

        assert_eq!(BlobFeePolicy::is_outbid(&tx, 6, GWEI as u64, &receipts, 21), expected);
    }

    /// A tx whose `max_fee_per_gas` leaves less than its `max_priority_fee_per_gas` above the
    /// base fee bids only what is left.
    #[test]
    fn a_tx_bids_the_tip_left_at_the_base_fee() {
        let tx = sent(6 * GWEI / 10, GWEI + 6 * GWEI / 100, 1);

        assert!(BlobFeePolicy::is_outbid(
            &tx,
            6,
            GWEI as u64,
            &vec![mined(GWEI / 20, 21_000, 1); 16],
            21
        ));
    }

    /// Clones of a policy, held by clones of the manager, share `p`.
    #[test]
    fn clones_share_the_carried_fee() {
        let policy = policy();
        let clone = policy.clone();

        clone.record_replacement(GWEI, 1);

        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), GWEI);
    }
}
