//! Fee policy for blob (EIP-4844) transactions.
//!
//! [`BlobFeePolicy`] prices each blob transaction from a `max_priority_fee_per_gas` per blob
//! that it carries from one blob transaction to the next, instead of asking the L1 node.

use std::sync::{Arc, Mutex};

use crate::FeeCalculator;

/// The three fee fields of a blob transaction, in wei.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobFees {
    /// Maximum priority fee per gas.
    pub max_priority_fee_per_gas: u128,
    /// Maximum total fee per gas (base fee and priority fee).
    pub max_fee_per_gas: u128,
    /// Maximum fee per blob gas.
    pub max_fee_per_blob_gas: u128,
}

/// Prices blob transactions from a `max_priority_fee_per_gas` per blob, `p`, carried from one
/// blob transaction to the next.
///
/// `p` starts at the lower bound. A transaction with `n` blobs is sent with a
/// `max_priority_fee_per_gas` of `p × n`, and fee caps of twice the current base fee and blob
/// base fee ([`fees`](Self::fees)). Each replacement doubles all three fields, as L1 nodes
/// require to replace a blob transaction, raises the fee caps further if current L1 prices
/// need it, and raises `p` to the new `max_priority_fee_per_gas` per blob
/// ([`replacement`](Self::replacement)). When a transaction lands, `p` drops to 90% of the
/// `max_priority_fee_per_gas` per blob it paid, never below the lower bound
/// ([`record_landed`](Self::record_landed)).
///
/// No `max_priority_fee_per_gas` per blob goes above the upper bound. A transaction whose
/// doubled `max_priority_fee_per_gas` would pass it is not replaced any more; it stays valid
/// at its last fees.
///
/// Clones share `p`, so every send of a manager and its clones uses the same value. It is
/// kept in memory only, and starts again from the lower bound after a restart.
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
    /// Percentage of the `max_priority_fee_per_gas` per blob of a landed transaction that the
    /// next blob transaction starts from.
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

    /// Returns the fees of a new transaction with `blob_count` blobs at the current L1
    /// `base_fee` and `blob_base_fee`.
    pub fn fees(&self, blob_count: u128, base_fee: u128, blob_base_fee: u128) -> BlobFees {
        let max_priority_fee_per_gas =
            self.max_priority_fee_per_gas_per_blob().saturating_mul(blob_count);
        BlobFees {
            max_priority_fee_per_gas,
            max_fee_per_gas: FeeCalculator::calc_gas_fee_cap(base_fee, max_priority_fee_per_gas),
            max_fee_per_blob_gas: FeeCalculator::calc_blob_fee_cap(blob_base_fee),
        }
    }

    /// Returns the fees that replace a transaction with `blob_count` blobs sent with `old`, at
    /// the current L1 `base_fee` and `blob_base_fee`, and raises `p` to the new
    /// `max_priority_fee_per_gas` per blob.
    ///
    /// Returns `None`, and leaves `p` unchanged, when the doubled `max_priority_fee_per_gas`
    /// would pass the upper bound: L1 nodes reject a replacement that does not double it.
    pub fn replacement(
        &self,
        old: &BlobFees,
        blob_count: u128,
        base_fee: u128,
        blob_base_fee: u128,
    ) -> Option<BlobFees> {
        let max_priority_fee_per_gas =
            FeeCalculator::calc_threshold_value(old.max_priority_fee_per_gas, true);
        if max_priority_fee_per_gas > self.upper_bound.saturating_mul(blob_count) {
            return None;
        }
        let max_fee_per_gas = FeeCalculator::calc_threshold_value(old.max_fee_per_gas, true)
            .max(FeeCalculator::calc_gas_fee_cap(base_fee, max_priority_fee_per_gas));
        let max_fee_per_blob_gas =
            FeeCalculator::calc_threshold_value(old.max_fee_per_blob_gas, true)
                .max(FeeCalculator::calc_blob_fee_cap(blob_base_fee));

        let mut p =
            self.max_priority_fee_per_gas_per_blob.lock().expect("BlobFeePolicy mutex poisoned");
        *p = (*p).max(max_priority_fee_per_gas / blob_count);

        Some(BlobFees { max_priority_fee_per_gas, max_fee_per_gas, max_fee_per_blob_gas })
    }

    /// Records that a transaction with `blob_count` blobs landed with
    /// `max_priority_fee_per_gas`: the next blob transaction starts from 90% of its
    /// `max_priority_fee_per_gas` per blob, never below the lower bound.
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
    use rstest::rstest;

    use super::*;

    const GWEI: u128 = 1_000_000_000;

    /// 0.01 to 10 gwei per blob.
    fn policy() -> BlobFeePolicy {
        BlobFeePolicy::new(GWEI / 100, 10 * GWEI)
    }

    #[rstest]
    #[case::one_blob(1)]
    #[case::six_blobs(6)]
    fn a_new_transaction_starts_at_the_lower_bound_per_blob(#[case] blob_count: u128) {
        assert_eq!(
            policy().fees(blob_count, 3 * GWEI, 5 * GWEI),
            BlobFees {
                max_priority_fee_per_gas: blob_count * GWEI / 100,
                max_fee_per_gas: blob_count * GWEI / 100 + 6 * GWEI,
                max_fee_per_blob_gas: 10 * GWEI,
            }
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
        let old = BlobFees {
            max_priority_fee_per_gas: 6 * GWEI,
            max_fee_per_gas: 16 * GWEI,
            max_fee_per_blob_gas: 20 * GWEI,
        };

        assert_eq!(
            policy().replacement(&old, 6, base_fee, blob_base_fee),
            Some(BlobFees {
                max_priority_fee_per_gas: expected_gwei.0 * GWEI,
                max_fee_per_gas: expected_gwei.1 * GWEI,
                max_fee_per_blob_gas: expected_gwei.2 * GWEI,
            })
        );
    }

    /// The next transactions start at least at the `max_priority_fee_per_gas` per blob of
    /// the last replacement, whatever their blob count.
    #[test]
    fn a_replacement_raises_the_next_transactions() {
        let policy = policy();
        let first = policy.fees(6, GWEI, GWEI);
        let replacement = policy.replacement(&first, 6, GWEI, GWEI).unwrap();

        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), 2 * GWEI / 100);
        assert_eq!(policy.fees(1, GWEI, GWEI).max_priority_fee_per_gas, 2 * GWEI / 100);
        assert_eq!(
            policy.fees(6, GWEI, GWEI).max_priority_fee_per_gas,
            replacement.max_priority_fee_per_gas
        );
    }

    /// A replacement of a transaction that is ahead of `p`, such as one sent before `p`
    /// dropped, never lowers `p`.
    #[test]
    fn a_replacement_never_lowers_the_next_transactions() {
        let policy = policy();
        policy.record_landed(60 * GWEI, 6);
        let old = BlobFees {
            max_priority_fee_per_gas: GWEI,
            max_fee_per_gas: 3 * GWEI,
            max_fee_per_blob_gas: GWEI,
        };

        policy.replacement(&old, 1, GWEI, GWEI).unwrap();

        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), 9 * GWEI);
    }

    /// The last replacement reaches at most the upper bound per blob. One whose doubled
    /// `max_priority_fee_per_gas` would pass it is not built, and `p` does not move.
    #[rstest]
    #[case::reaches_the_bound(30 * GWEI, Some(60 * GWEI))]
    #[case::would_pass_the_bound(30 * GWEI + 1, None)]
    #[case::at_the_bound(60 * GWEI, None)]
    fn a_replacement_stays_within_the_upper_bound(
        #[case] max_priority_fee_per_gas: u128,
        #[case] expected: Option<u128>,
    ) {
        let policy = policy();
        let old = BlobFees {
            max_priority_fee_per_gas,
            max_fee_per_gas: max_priority_fee_per_gas + 2 * GWEI,
            max_fee_per_blob_gas: 2 * GWEI,
        };

        let replacement = policy.replacement(&old, 6, GWEI, GWEI);

        assert_eq!(replacement.map(|fees| fees.max_priority_fee_per_gas), expected);
        let expected_p = expected.map_or(GWEI / 100, |fee| fee / 6);
        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), expected_p);
    }

    /// After a landed transaction, the next one starts at 90% of its
    /// `max_priority_fee_per_gas` per blob, never below the lower bound, even when `p` was
    /// higher.
    #[rstest]
    #[case::decays_by_ten_percent(3_840_000_000, 6, 576_000_000)]
    #[case::per_blob(GWEI, 2, 450_000_000)]
    #[case::stops_at_the_lower_bound(GWEI / 100, 1, GWEI / 100)]
    fn a_landed_transaction_sets_the_next_one_to_ninety_percent(
        #[case] max_priority_fee_per_gas: u128,
        #[case] blob_count: u128,
        #[case] expected_p: u128,
    ) {
        let policy = policy();
        policy.replacement(&policy.fees(1, GWEI, GWEI), 1, GWEI, GWEI).unwrap();
        policy.replacement(&policy.fees(1, GWEI, GWEI), 1, GWEI, GWEI).unwrap();

        policy.record_landed(max_priority_fee_per_gas, blob_count);

        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), expected_p);
    }

    /// The example of the policy: a 6-blob transaction at 0.06 gwei while builders need about
    /// 3 gwei doubles six times, lands at 3.84 gwei, and the next ones start 10% lower each.
    #[test]
    fn a_congestion_climbs_then_decays() {
        let policy = policy();
        let mut fees = policy.fees(6, GWEI, GWEI);
        let mut climb = vec![fees.max_priority_fee_per_gas];
        while fees.max_priority_fee_per_gas < 3 * GWEI {
            fees = policy.replacement(&fees, 6, GWEI, GWEI).unwrap();
            climb.push(fees.max_priority_fee_per_gas);
        }
        policy.record_landed(fees.max_priority_fee_per_gas, 6);
        let mut next = Vec::new();
        for _ in 0..3 {
            let fees = policy.fees(6, GWEI, GWEI);
            next.push(fees.max_priority_fee_per_gas);
            policy.record_landed(fees.max_priority_fee_per_gas, 6);
        }

        assert_eq!(
            climb,
            [60, 120, 240, 480, 960, 1_920, 3_840].map(|mwei| mwei * 1_000_000).to_vec()
        );
        assert_eq!(next, [3_456_000_000, 3_110_400_000, 2_799_360_000]);
    }

    #[test]
    fn clones_share_the_carried_max_priority_fee_per_gas_per_blob() {
        let policy = policy();
        let clone = policy.clone();

        clone.record_landed(6 * GWEI, 6);

        assert_eq!(policy.max_priority_fee_per_gas_per_blob(), 9 * GWEI / 10);
    }
}
