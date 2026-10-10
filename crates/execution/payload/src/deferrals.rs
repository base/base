//! Per-block dedupe of repeated builder decision events.
use alloy_primitives::{TxHash, map::B256Map};

/// The defer reason last journaled as `BUILDER_DEFERRED` for each transaction in the block being
/// built.
///
/// A validity-gated transaction that stays blocked is parked again after every promotion, and by
/// the flashblocks builder on every flashblock, so builders report a deferral only when it tells
/// the journal something new. Create one per block build; it is never pruned during the build.
#[derive(Debug, Default)]
pub struct BlockDeferrals {
    reasons: B256Map<&'static str>,
}

impl BlockDeferrals {
    /// Records that `tx_hash` was deferred for `reason`, returning `true` when this is its first
    /// deferral in the block or the reason differs from the last one recorded.
    pub fn record(&mut self, tx_hash: TxHash, reason: &'static str) -> bool {
        self.reasons.insert(tx_hash, reason) != Some(reason)
    }
}

/// The reasons a transaction has been rejected for in the block being built.
///
/// A transaction that overflows a flashblock's cumulative gas, DA, or size budget is rejected
/// again on every later flashblock, so builders report a rejection only the first time the block's
/// build rejects it for that reason. Unlike [`BlockDeferrals`], every reason seen for a
/// transaction is remembered, not only the last one, so `gas -> DA -> gas` reports two rejections
/// for two reasons rather than three. One bit per reason code keeps the state bounded: create one
/// per block build and drop it when the block is sealed; a rejection in the next block is a new
/// event because the journal starts empty.
#[derive(Debug, Default)]
pub struct BlockRejections {
    reasons: B256Map<u32>,
}

impl BlockRejections {
    /// Records that `tx_hash` was rejected for `reason`, returning `true` when the block's build
    /// has not rejected the transaction for that reason yet.
    pub fn record(&mut self, tx_hash: TxHash, reason: &'static str) -> bool {
        let Some(bit) = Self::reason_bit(reason) else {
            // A code this journal does not know is never suppressed, so a code added to the
            // builder keeps reaching the journal until it is registered here.
            return true;
        };
        let seen = self.reasons.entry(tx_hash).or_default();
        if *seen & bit != 0 {
            return false;
        }
        *seen |= bit;
        true
    }

    /// The bit this journal uses for `reason`, or `None` when the code is not one of the
    /// `BUILDER_REJECTED` reasons the flashblocks builder emits: the six reason codes it records
    /// through its `DecisionContext`, and the twelve `rejection_reason_code` maps a
    /// `TxnExecutionError` to.
    fn reason_bit(reason: &str) -> Option<u32> {
        let bit = match reason {
            "validity_predicate_read_failed" => 0,
            "validity_predicate_expired" => 1,
            "validity_predicate_not_satisfied" => 2,
            "manifest_precheck_stale" => 3,
            "unschedulable_payer_authenticator" => 4,
            "unaffordable_coinbase_tip" => 5,
            "tx_da_size_exceeded" => 6,
            "block_da_size_exceeded" => 7,
            "da_footprint_limit_exceeded" => 8,
            "transaction_gas_limit_exceeded" => 9,
            "block_uncompressed_size_exceeded" => 10,
            "tx_execution_time_exceeded" => 11,
            "sequencer_transaction" => 12,
            "nonce_too_low" => 13,
            "internal_error" => 14,
            "evm_error" => 15,
            "max_gas_usage_exceeded" => 16,
            "metering_data_pending" => 17,
            _ => return None,
        };
        Some(1 << bit)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    /// Every `BUILDER_REJECTED` reason code the flashblocks builder emits, so the journal's bit
    /// table is checked against the codes that reach it.
    const REJECTION_REASON_CODES: [&str; 18] = [
        "validity_predicate_read_failed",
        "validity_predicate_expired",
        "validity_predicate_not_satisfied",
        "manifest_precheck_stale",
        "unschedulable_payer_authenticator",
        "unaffordable_coinbase_tip",
        "tx_da_size_exceeded",
        "block_da_size_exceeded",
        "da_footprint_limit_exceeded",
        "transaction_gas_limit_exceeded",
        "block_uncompressed_size_exceeded",
        "tx_execution_time_exceeded",
        "sequencer_transaction",
        "nonce_too_low",
        "internal_error",
        "evm_error",
        "max_gas_usage_exceeded",
        "metering_data_pending",
    ];

    #[test]
    fn repeated_deferral_for_the_same_reason_is_recorded_once() {
        let mut deferrals = BlockDeferrals::default();
        let tx = B256::repeat_byte(1);

        assert!(deferrals.record(tx, "validity_predicate_not_satisfied"));
        assert!(!deferrals.record(tx, "validity_predicate_not_satisfied"));
        assert!(deferrals.record(B256::repeat_byte(2), "validity_predicate_not_satisfied"));
    }

    #[test]
    fn a_changed_reason_is_recorded_again() {
        let mut deferrals = BlockDeferrals::default();
        let tx = B256::repeat_byte(1);

        assert!(deferrals.record(tx, "validity_predicate_not_satisfied"));
        assert!(deferrals.record(tx, "predicate_eval_budget_exhausted"));
        assert!(!deferrals.record(tx, "predicate_eval_budget_exhausted"));
        assert!(deferrals.record(tx, "validity_predicate_not_satisfied"));
    }

    #[test]
    fn repeated_rejection_for_the_same_reason_is_recorded_once() {
        let mut rejections = BlockRejections::default();
        let tx = B256::repeat_byte(1);

        assert!(rejections.record(tx, "transaction_gas_limit_exceeded"));
        assert!(!rejections.record(tx, "transaction_gas_limit_exceeded"));
        assert!(rejections.record(B256::repeat_byte(2), "transaction_gas_limit_exceeded"));
    }

    /// A transaction can be rejected for one reason, then another, then the first again, all
    /// within one block. Each reason is reported once, so returning to an earlier reason does not
    /// emit again.
    #[test]
    fn alternating_rejection_reasons_are_each_recorded_once() {
        let mut rejections = BlockRejections::default();
        let tx = B256::repeat_byte(1);

        assert!(rejections.record(tx, "transaction_gas_limit_exceeded"));
        assert!(rejections.record(tx, "block_da_size_exceeded"));
        assert!(!rejections.record(tx, "transaction_gas_limit_exceeded"));
        assert!(!rejections.record(tx, "block_da_size_exceeded"));
    }

    /// Each reason code owns a distinct bit, so a transaction rejected for many reasons in one
    /// block keeps all of them and repeats none.
    #[test]
    fn every_rejection_reason_code_has_its_own_bit() {
        let mut rejections = BlockRejections::default();
        let tx = B256::repeat_byte(1);

        for code in REJECTION_REASON_CODES {
            assert!(rejections.record(tx, code), "first {code} must be recorded");
        }
        for code in REJECTION_REASON_CODES {
            assert!(!rejections.record(tx, code), "repeat {code} must be suppressed");
        }
    }

    /// A code the journal does not know is never suppressed, so a new builder reason keeps
    /// reaching the journal instead of being silently dropped.
    #[test]
    fn an_unregistered_rejection_reason_code_is_never_suppressed() {
        let mut rejections = BlockRejections::default();
        let tx = B256::repeat_byte(1);

        assert!(rejections.record(tx, "not_a_registered_reason"));
        assert!(rejections.record(tx, "not_a_registered_reason"));
    }

    #[test]
    fn a_fresh_journal_records_the_reason_again_for_the_next_block() {
        let tx = B256::repeat_byte(1);

        let mut block_n = BlockRejections::default();
        assert!(block_n.record(tx, "transaction_gas_limit_exceeded"));
        assert!(!block_n.record(tx, "transaction_gas_limit_exceeded"));

        // The next block build starts an empty journal, so the same rejection is reported again.
        let mut block_n_plus_1 = BlockRejections::default();
        assert!(block_n_plus_1.record(tx, "transaction_gas_limit_exceeded"));
    }
}
