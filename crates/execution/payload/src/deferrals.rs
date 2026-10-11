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

/// The `rejection_reason` codes a builder journals in `BUILDER_REJECTED`, and the registry for
/// per-block rejection dedupe.
///
/// [`BlockRejections`] suppresses a code only when it is in [`RejectionReason::ALL`], and both
/// builders emit these constants instead of string literals, so a new reason cannot reach the
/// journal without being registered for dedupe. Besides every code either builder emits, the
/// registry keeps three codes that share the vocabulary but currently route to another event
/// (`BUILDER_DEFERRED` or `BUILDER_EXPIRED`) or to no event at all.
#[derive(Debug)]
pub struct RejectionReason;

impl RejectionReason {
    /// A validity predicate's state read failed.
    pub const VALIDITY_PREDICATE_READ_FAILED: &'static str = "validity_predicate_read_failed";
    /// A validity predicate can no longer be satisfied at or after the current build position.
    pub const VALIDITY_PREDICATE_EXPIRED: &'static str = "validity_predicate_expired";
    /// A validity predicate is not satisfied by the current build state.
    pub const VALIDITY_PREDICATE_NOT_SATISFIED: &'static str = "validity_predicate_not_satisfied";
    /// An EIP-8130 authorization manifest was stale at the build position.
    pub const MANIFEST_PRECHECK_STALE: &'static str = "manifest_precheck_stale";
    /// An EIP-8130 payer authenticator cannot be scheduled against the gas budget.
    pub const UNSCHEDULABLE_PAYER_AUTHENTICATOR: &'static str = "unschedulable_payer_authenticator";
    /// The sender and gas payer cannot cover worst-case gas plus the declared coinbase tip.
    pub const UNAFFORDABLE_COINBASE_TIP: &'static str = "unaffordable_coinbase_tip";
    /// The transaction's DA size exceeds the per-transaction limit.
    pub const TX_DA_SIZE_EXCEEDED: &'static str = "tx_da_size_exceeded";
    /// The block's DA size limit was exceeded.
    pub const BLOCK_DA_SIZE_EXCEEDED: &'static str = "block_da_size_exceeded";
    /// The block's DA footprint limit was exceeded.
    pub const DA_FOOTPRINT_LIMIT_EXCEEDED: &'static str = "da_footprint_limit_exceeded";
    /// The transaction's gas limit exceeds the remaining block gas.
    pub const TRANSACTION_GAS_LIMIT_EXCEEDED: &'static str = "transaction_gas_limit_exceeded";
    /// The block's uncompressed size limit was exceeded.
    pub const BLOCK_UNCOMPRESSED_SIZE_EXCEEDED: &'static str = "block_uncompressed_size_exceeded";
    /// The transaction's predicted execution time exceeded its per-transaction limit.
    pub const TX_EXECUTION_TIME_EXCEEDED: &'static str = "tx_execution_time_exceeded";
    /// The transaction is a sequencer transaction that cannot be scheduled.
    pub const SEQUENCER_TRANSACTION: &'static str = "sequencer_transaction";
    /// The transaction nonce is below the account nonce.
    pub const NONCE_TOO_LOW: &'static str = "nonce_too_low";
    /// The builder hit an internal error handling the transaction.
    pub const INTERNAL_ERROR: &'static str = "internal_error";
    /// The transaction failed EVM validation in the flashblocks builder.
    pub const EVM_ERROR: &'static str = "evm_error";
    /// The transaction would exceed the block's maximum gas usage.
    pub const MAX_GAS_USAGE_EXCEEDED: &'static str = "max_gas_usage_exceeded";
    /// Resource metering data for the transaction has not arrived yet.
    pub const METERING_DATA_PENDING: &'static str = "metering_data_pending";
    /// The native builder does not support flashblock-index predicates.
    pub const UNSUPPORTED_FLASHBLOCK_INDEX_PREDICATE: &'static str =
        "unsupported_flashblock_index_predicate";
    /// The transaction's simulated or executed resource usage exceeds the metering budget.
    pub const RESOURCE_METERING_EXCLUDED: &'static str = "resource_metering_excluded";
    /// The transaction does not fit the remaining block gas or DA budget.
    pub const BLOCK_LIMITS_EXCEEDED: &'static str = "block_limits_exceeded";
    /// The transaction failed EVM validation in the native builder.
    pub const INVALID_TRANSACTION: &'static str = "invalid_transaction";

    /// Every registered code, indexed by the bit it owns in a block's rejection set.
    pub const ALL: [&'static str; 22] = [
        Self::VALIDITY_PREDICATE_READ_FAILED,
        Self::VALIDITY_PREDICATE_EXPIRED,
        Self::VALIDITY_PREDICATE_NOT_SATISFIED,
        Self::MANIFEST_PRECHECK_STALE,
        Self::UNSCHEDULABLE_PAYER_AUTHENTICATOR,
        Self::UNAFFORDABLE_COINBASE_TIP,
        Self::TX_DA_SIZE_EXCEEDED,
        Self::BLOCK_DA_SIZE_EXCEEDED,
        Self::DA_FOOTPRINT_LIMIT_EXCEEDED,
        Self::TRANSACTION_GAS_LIMIT_EXCEEDED,
        Self::BLOCK_UNCOMPRESSED_SIZE_EXCEEDED,
        Self::TX_EXECUTION_TIME_EXCEEDED,
        Self::SEQUENCER_TRANSACTION,
        Self::NONCE_TOO_LOW,
        Self::INTERNAL_ERROR,
        Self::EVM_ERROR,
        Self::MAX_GAS_USAGE_EXCEEDED,
        Self::METERING_DATA_PENDING,
        Self::UNSUPPORTED_FLASHBLOCK_INDEX_PREDICATE,
        Self::RESOURCE_METERING_EXCLUDED,
        Self::BLOCK_LIMITS_EXCEEDED,
        Self::INVALID_TRANSACTION,
    ];
}

/// A block's rejection set is one bit per registered reason, so the registry must fit a `u32`.
const _: () = assert!(RejectionReason::ALL.len() <= u32::BITS as usize);

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
            // A code outside the registry is never suppressed, so a new builder reason keeps
            // reaching the journal instead of being silently dropped.
            return true;
        };
        let seen = self.reasons.entry(tx_hash).or_default();
        if *seen & bit != 0 {
            return false;
        }
        *seen |= bit;
        true
    }

    /// The bit `reason` owns in the block's rejection set, or `None` when it is not registered in
    /// [`RejectionReason::ALL`].
    fn reason_bit(reason: &str) -> Option<u32> {
        RejectionReason::ALL.iter().position(|code| *code == reason).map(|bit| 1u32 << bit)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

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

        for code in RejectionReason::ALL {
            assert!(rejections.record(tx, code), "first {code} must be recorded");
        }
        for code in RejectionReason::ALL {
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
