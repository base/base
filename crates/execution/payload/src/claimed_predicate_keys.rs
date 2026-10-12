//! Block-scoped claims on the state that committed validity transactions conditioned on.

use alloy_primitives::map::HashSet;
use base_execution_txpool::ValidityPredicate;

use crate::ValidityPredicateKey;

/// State locations read by the predicates of validity transactions committed in the current block.
///
/// A validity transaction bids on a state transition described by its balance, nonce, and storage
/// predicates. Once one such transaction commits, the opportunity it conditioned on is treated as
/// captured. A later transaction flagged
/// [`fill_immediately_or_kill`](base_execution_txpool::TransactionValidity::fill_immediately_or_kill)
/// that conditions on any of the same locations is [`Self::contended`] and dropped instead of
/// included, even when its predicates still hold. This approximates revert protection without
/// executing and rolling back the transaction.
///
/// Block-number and flashblock-index predicates are shared timing context, not contested state, so
/// they never claim a location. Create one per block build.
#[derive(Debug, Default)]
pub struct ClaimedPredicateKeys {
    keys: HashSet<ValidityPredicateKey>,
}

impl ClaimedPredicateKeys {
    /// Claims the state locations read by a committed transaction's predicates.
    pub fn claim(&mut self, predicates: &[ValidityPredicate]) {
        self.keys.extend(Self::state_keys(predicates));
    }

    /// Returns whether a committed validity transaction already claimed a state location that
    /// `predicates` read.
    pub fn contended(&self, predicates: &[ValidityPredicate]) -> bool {
        !self.keys.is_empty() && Self::state_keys(predicates).any(|key| self.keys.contains(&key))
    }

    fn state_keys(
        predicates: &[ValidityPredicate],
    ) -> impl Iterator<Item = ValidityPredicateKey> + '_ {
        predicates.iter().map(ValidityPredicateKey::for_predicate).filter(|key| key.is_state())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_execution_txpool::ValidityOperator;

    use super::*;

    fn storage(slot: u64, value: u64) -> ValidityPredicate {
        ValidityPredicate::Storage {
            address: Address::repeat_byte(0x11),
            slot: U256::from(slot),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(value),
        }
    }

    fn block_at_most(value: u64) -> ValidityPredicate {
        ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(value),
        }
    }

    #[test]
    fn nothing_is_contended_before_any_claim() {
        assert!(!ClaimedPredicateKeys::default().contended(&[storage(1, 5)]));
    }

    #[test]
    fn same_location_is_contended_regardless_of_comparison() {
        let mut claimed = ClaimedPredicateKeys::default();
        claimed.claim(&[storage(1, 5)]);

        assert!(claimed.contended(&[storage(1, 9)]));
    }

    #[test]
    fn disjoint_locations_are_not_contended() {
        let mut claimed = ClaimedPredicateKeys::default();
        claimed.claim(&[storage(1, 5)]);

        assert!(!claimed.contended(&[storage(2, 5)]));
        let balance = ValidityPredicate::Balance {
            address: Address::repeat_byte(0x11),
            op: ValidityOperator::GreaterThan,
            value: U256::ZERO,
        };
        assert!(!claimed.contended(&[balance]));
    }

    #[test]
    fn shared_block_bounds_do_not_contend() {
        let mut claimed = ClaimedPredicateKeys::default();
        claimed.claim(&[block_at_most(100), storage(1, 5)]);

        assert!(!claimed.contended(&[block_at_most(100), storage(2, 5)]));
    }
}
