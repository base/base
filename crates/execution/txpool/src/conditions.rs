//! Ingested validity conditions, partitioned into timing bounds and state predicates.

use std::{borrow::Cow, sync::OnceLock};

use alloy_primitives::U256;

use crate::{PredicateContext, ValidityOperator, ValidityPredicate};

/// Inclusive timing bounds compiled once from a conjunction of predicates.
///
/// `!=` cannot be represented by min/max bounds and remains an explicit exclusion.
/// Strict comparisons at the ends of the U256 domain remain unsatisfiable rather
/// than wrapping or becoming a satisfiable inclusive bound.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidityBounds {
    min: Option<U256>,
    max: Option<U256>,
    excluded: Vec<U256>,
    below_zero: bool,
    above_max: bool,
    present: bool,
}

impl ValidityBounds {
    /// Adds one timing comparison to this conjunction.
    pub fn insert(&mut self, op: ValidityOperator, value: U256) {
        self.present = true;
        match op {
            ValidityOperator::LessThan => match value.checked_sub(U256::from(1)) {
                Some(max) => self.max = Some(self.max.map_or(max, |old| old.min(max))),
                None => self.below_zero = true,
            },
            ValidityOperator::LessThanOrEqual => {
                self.max = Some(self.max.map_or(value, |old| old.min(value)));
            }
            ValidityOperator::Equal => {
                self.min = Some(self.min.map_or(value, |old| old.max(value)));
                self.max = Some(self.max.map_or(value, |old| old.min(value)));
            }
            ValidityOperator::NotEqual => self.excluded.push(value),
            ValidityOperator::GreaterThan => match value.checked_add(U256::from(1)) {
                Some(min) => self.min = Some(self.min.map_or(min, |old| old.max(min))),
                None => self.above_max = true,
            },
            ValidityOperator::GreaterThanOrEqual => {
                self.min = Some(self.min.map_or(value, |old| old.max(value)));
            }
        }
    }

    /// Returns whether any predicate for this timing target was submitted.
    pub const fn is_present(&self) -> bool {
        self.present
    }

    /// Returns the tightest inclusive lower bound.
    pub const fn min(&self) -> Option<U256> {
        self.min
    }

    /// Returns the tightest inclusive upper bound.
    pub const fn max(&self) -> Option<U256> {
        self.max
    }

    /// Returns the first failed normalized comparison, without materializing predicates.
    ///
    /// Impossible strict endpoints precede the lower bound, upper bound, and exclusions.
    pub fn first_unsatisfied(&self, value: U256) -> Option<(ValidityOperator, U256)> {
        if self.below_zero {
            return Some((ValidityOperator::LessThan, U256::ZERO));
        }
        if self.above_max {
            return Some((ValidityOperator::GreaterThan, U256::MAX));
        }
        if let Some(min) = self.min
            && value < min
        {
            return Some((ValidityOperator::GreaterThanOrEqual, min));
        }
        if let Some(max) = self.max
            && value > max
        {
            return Some((ValidityOperator::LessThanOrEqual, max));
        }
        self.excluded.contains(&value).then_some((ValidityOperator::NotEqual, value))
    }

    /// Whether a comparison is part of this conjunction's normalized representation.
    ///
    /// This checks blocker identity, not whether the comparison is implied by the bounds.
    pub fn contains(&self, op: ValidityOperator, value: U256) -> bool {
        match op {
            ValidityOperator::LessThan => self.below_zero && value == U256::ZERO,
            ValidityOperator::GreaterThan => self.above_max && value == U256::MAX,
            ValidityOperator::GreaterThanOrEqual => self.min == Some(value),
            ValidityOperator::LessThanOrEqual => self.max == Some(value),
            ValidityOperator::NotEqual => self.excluded.contains(&value),
            ValidityOperator::Equal => false,
        }
    }

    /// Returns comparisons equivalent to the ingested conjunction.
    pub fn comparisons(&self) -> impl Iterator<Item = (ValidityOperator, U256)> + '_ {
        self.below_zero
            .then_some((ValidityOperator::LessThan, U256::ZERO))
            .into_iter()
            .chain(self.above_max.then_some((ValidityOperator::GreaterThan, U256::MAX)))
            .chain(self.min.map(|min| (ValidityOperator::GreaterThanOrEqual, min)))
            .chain(self.max.map(|max| (ValidityOperator::LessThanOrEqual, max)))
            .chain(self.excluded.iter().map(|value| (ValidityOperator::NotEqual, *value)))
    }

    /// Returns the pool's finite, inclusive expiry bound without rescanning predicates.
    pub fn expiry_bound(&self) -> Option<u64> {
        if self.below_zero {
            return Some(0);
        }
        self.max.and_then(|max| u64::try_from(max).ok())
    }

    /// Heap bytes used by exclusions.
    pub const fn heap_size(&self) -> usize {
        self.excluded.capacity() * core::mem::size_of::<U256>()
    }
}

/// Conditions compiled at ingress for repeated pool and builder checks.
///
/// Timing comparisons are reduced to bounds. State predicates are partitioned
/// by type; timing always gates state reads. The submitted count is retained
/// because transaction ordering uses it, even when redundant bounds collapse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidityConditions {
    block: ValidityBounds,
    flashblock: ValidityBounds,
    balance: Vec<ValidityPredicate>,
    nonce: Vec<ValidityPredicate>,
    storage: Vec<ValidityPredicate>,
    /// Original wire batch, used only when forwarding to a builder.
    submitted: Vec<ValidityPredicate>,
}

impl ValidityConditions {
    /// Compiles submitted predicates without modifying the wire representation.
    pub fn new(predicates: Vec<ValidityPredicate>) -> Self {
        // Count partitions before cloning large predicates so larger batches do not
        // repeatedly grow buffers, and small partitions do not retain minimum Vec capacity.
        let mut balance_count = 0;
        let mut nonce_count = 0;
        let mut storage_count = 0;
        let mut block_exclusions = 0;
        let mut flashblock_exclusions = 0;
        for predicate in &predicates {
            match predicate {
                ValidityPredicate::Balance { .. } => balance_count += 1,
                ValidityPredicate::Nonce { .. } => nonce_count += 1,
                ValidityPredicate::Storage { .. } => storage_count += 1,
                ValidityPredicate::BlockNumber { op: ValidityOperator::NotEqual, .. } => {
                    block_exclusions += 1;
                }
                ValidityPredicate::FlashblockIndex { op: ValidityOperator::NotEqual, .. } => {
                    flashblock_exclusions += 1;
                }
                _ => {}
            }
        }
        let mut conditions = Self { submitted: predicates, ..Self::default() };
        conditions.balance.reserve_exact(balance_count);
        conditions.nonce.reserve_exact(nonce_count);
        conditions.storage.reserve_exact(storage_count);
        conditions.block.excluded.reserve_exact(block_exclusions);
        conditions.flashblock.excluded.reserve_exact(flashblock_exclusions);
        for predicate in &conditions.submitted {
            match predicate {
                ValidityPredicate::BlockNumber { op, value } => {
                    conditions.block.insert(*op, *value)
                }
                ValidityPredicate::FlashblockIndex { op, value } => {
                    conditions.flashblock.insert(*op, *value)
                }
                ValidityPredicate::Balance { .. } => conditions.balance.push(predicate.clone()),
                ValidityPredicate::Nonce { .. } => conditions.nonce.push(predicate.clone()),
                ValidityPredicate::Storage { .. } => conditions.storage.push(predicate.clone()),
            }
        }
        conditions
    }

    /// Shared empty conditions for transaction types without validity metadata.
    pub fn empty() -> &'static Self {
        static EMPTY: OnceLock<ValidityConditions> = OnceLock::new();
        EMPTY.get_or_init(Self::default)
    }

    /// Original predicates for lossless builder forwarding; never used for evaluation.
    pub const fn submitted(&self) -> &[ValidityPredicate] {
        self.submitted.as_slice()
    }

    /// Block-number bounds.
    pub const fn block(&self) -> &ValidityBounds {
        &self.block
    }

    /// Flashblock-index bounds, whose indices reset each block.
    pub const fn flashblock(&self) -> &ValidityBounds {
        &self.flashblock
    }

    /// Number of submitted predicates, before normalization.
    pub const fn len(&self) -> usize {
        self.submitted.len()
    }

    /// Whether the transaction carries no validity conditions.
    pub const fn is_empty(&self) -> bool {
        self.submitted.is_empty()
    }

    /// State predicates in balance, nonce, then storage order.
    pub fn state_predicates(&self) -> impl Iterator<Item = &ValidityPredicate> {
        self.balance.iter().chain(&self.nonce).chain(&self.storage)
    }

    /// Normalized predicates in evaluation order, for parking and resting checks.
    ///
    /// Timing predicates are synthesized; state predicates are borrowed so successful
    /// evaluation and membership checks do not clone them.
    pub fn iter(&self) -> impl Iterator<Item = Cow<'_, ValidityPredicate>> + '_ {
        self.block
            .comparisons()
            .map(|(op, value)| Cow::Owned(ValidityPredicate::BlockNumber { op, value }))
            .chain(
                self.flashblock.comparisons().map(|(op, value)| {
                    Cow::Owned(ValidityPredicate::FlashblockIndex { op, value })
                }),
            )
            .chain(self.state_predicates().map(Cow::Borrowed))
    }

    /// Whether these conditions contain the normalized predicate recorded by a resting iterator.
    pub fn contains(&self, predicate: &ValidityPredicate) -> bool {
        match predicate {
            ValidityPredicate::BlockNumber { op, value } => self.block.contains(*op, *value),
            ValidityPredicate::FlashblockIndex { op, value } => {
                self.flashblock.contains(*op, *value)
            }
            ValidityPredicate::Balance { .. } => self.balance.contains(predicate),
            ValidityPredicate::Nonce { .. } => self.nonce.contains(predicate),
            ValidityPredicate::Storage { .. } => self.storage.contains(predicate),
        }
    }

    /// Whether a passed timing upper bound proves permanent expiry.
    ///
    /// A flashblock deadline is terminal only in the last allowed block. State
    /// mismatches and contradictory lower bounds do not change expiry policy.
    pub fn is_expired(&self, context: &PredicateContext) -> bool {
        let block = U256::from(context.block_number);
        self.block.below_zero
            || self.flashblock.below_zero
            || self.block.max.is_some_and(|max| max < block)
            || (self.block.max == Some(block)
                && self
                    .flashblock
                    .max
                    .is_some_and(|max| max < U256::from(context.flashblock_index)))
    }

    /// Heap bytes used by the partitioned conditions.
    pub const fn heap_size(&self) -> usize {
        self.block.heap_size()
            + self.flashblock.heap_size()
            + (self.balance.capacity()
                + self.nonce.capacity()
                + self.storage.capacity()
                + self.submitted.capacity())
                * core::mem::size_of::<ValidityPredicate>()
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use alloy_primitives::{Address, U256};
    use revm::database::InMemoryDB;

    use super::{ValidityBounds, ValidityConditions};
    use crate::{
        PredicateContext, ValidityOperator,
        ValidityOperator::{
            Equal, GreaterThan, GreaterThanOrEqual, LessThan, LessThanOrEqual, NotEqual,
        },
        ValidityPredicate,
    };

    #[test]
    fn compiled_comparisons_cover_operators_and_full_width_boundaries() {
        let beyond_context = U256::from(u64::MAX) + U256::ONE;
        for (op, threshold, positions, expected) in [
            (LessThan, U256::from(2), [1, 2, 3], [true, false, false]),
            (LessThanOrEqual, U256::from(2), [1, 2, 3], [true, true, false]),
            (Equal, U256::from(2), [1, 2, 3], [false, true, false]),
            (NotEqual, U256::from(2), [1, 2, 3], [true, false, true]),
            (GreaterThan, U256::from(2), [1, 2, 3], [false, false, true]),
            (GreaterThanOrEqual, U256::from(2), [1, 2, 3], [false, true, true]),
            (LessThan, U256::ZERO, [0, 1, u64::MAX], [false; 3]),
            (LessThanOrEqual, U256::ZERO, [0, 1, u64::MAX], [true, false, false]),
            (GreaterThan, U256::MAX, [0, 1, u64::MAX], [false; 3]),
            (GreaterThanOrEqual, U256::MAX, [0, 1, u64::MAX], [false; 3]),
            (Equal, beyond_context, [0, 1, u64::MAX], [false; 3]),
            (GreaterThanOrEqual, beyond_context, [0, 1, u64::MAX], [false; 3]),
            (LessThan, beyond_context, [0, 1, u64::MAX], [true; 3]),
            (LessThanOrEqual, U256::MAX, [0, 1, u64::MAX], [true; 3]),
            (NotEqual, U256::MAX, [0, 1, u64::MAX], [true; 3]),
        ] {
            let mut bounds = ValidityBounds::default();
            bounds.insert(op, threshold);
            for (position, matches) in positions.into_iter().zip(expected) {
                assert_eq!(
                    bounds.first_unsatisfied(U256::from(position)).is_none(),
                    matches,
                    "{op:?} {threshold} at {position}"
                );
            }
        }
        let mut bounds = ValidityBounds::default();
        assert_eq!(bounds.first_unsatisfied(U256::ZERO), None);
        assert_eq!(bounds.first_unsatisfied(U256::MAX), None);
        bounds.insert(Equal, U256::MAX);
        assert_eq!(bounds.first_unsatisfied(U256::MAX), None);
    }

    #[test]
    fn normalized_membership_is_exact_not_logical_implication() {
        for flashblock in [false, true] {
            let predicate = |op, value| {
                if flashblock {
                    ValidityPredicate::FlashblockIndex { op, value }
                } else {
                    ValidityPredicate::BlockNumber { op, value }
                }
            };
            let conditions = ValidityConditions::new(vec![
                predicate(GreaterThan, U256::from(9)),
                predicate(LessThan, U256::from(21)),
                predicate(Equal, U256::from(15)),
                predicate(NotEqual, U256::from(15)),
                predicate(NotEqual, U256::from(15)),
                predicate(LessThan, U256::ZERO),
                predicate(GreaterThan, U256::MAX),
            ]);
            for (op, value) in [
                (GreaterThanOrEqual, U256::from(15)),
                (LessThanOrEqual, U256::from(15)),
                (NotEqual, U256::from(15)),
                (LessThan, U256::ZERO),
                (GreaterThan, U256::MAX),
            ] {
                assert!(conditions.contains(&predicate(op, value)));
            }
            for (op, value) in [
                (Equal, U256::from(15)),
                (GreaterThan, U256::from(9)),
                (LessThan, U256::from(21)),
                (GreaterThanOrEqual, U256::from(10)),
                (LessThanOrEqual, U256::from(20)),
                (NotEqual, U256::from(14)),
            ] {
                assert!(!conditions.contains(&predicate(op, value)));
            }
            // The other timing partition is absent, even at the same threshold.
            assert!(!conditions.contains(&if flashblock {
                ValidityPredicate::BlockNumber { op: NotEqual, value: U256::from(15) }
            } else {
                ValidityPredicate::FlashblockIndex { op: NotEqual, value: U256::from(15) }
            }));
        }
    }

    #[test]
    fn first_blocker_preserves_impossible_bound_and_exclusion_order() {
        let mut bounds = ValidityBounds::default();
        bounds.insert(NotEqual, U256::from(5));
        bounds.insert(NotEqual, U256::from(5));
        bounds.insert(GreaterThanOrEqual, U256::from(5));
        bounds.insert(LessThanOrEqual, U256::from(3));
        assert_eq!(
            bounds.first_unsatisfied(U256::from(4)),
            Some((GreaterThanOrEqual, U256::from(5)))
        );
        assert_eq!(bounds.first_unsatisfied(U256::from(5)), Some((LessThanOrEqual, U256::from(3))));
        bounds.insert(GreaterThan, U256::MAX);
        assert_eq!(bounds.first_unsatisfied(U256::from(4)), Some((GreaterThan, U256::MAX)));
        bounds.insert(LessThan, U256::ZERO);
        assert_eq!(bounds.first_unsatisfied(U256::from(4)), Some((LessThan, U256::ZERO)));

        let mut exclusions = ValidityBounds::default();
        exclusions.insert(NotEqual, U256::from(5));
        exclusions.insert(NotEqual, U256::from(5));
        exclusions.insert(NotEqual, U256::from(6));
        assert_eq!(exclusions.first_unsatisfied(U256::from(5)), Some((NotEqual, U256::from(5))));
        assert_eq!(exclusions.first_unsatisfied(U256::from(6)), Some((NotEqual, U256::from(6))));
        assert_eq!(exclusions.first_unsatisfied(U256::from(7)), None);
    }

    #[test]
    fn heap_size_counts_spare_capacity_in_every_backing_vector() {
        let mut conditions = ValidityConditions {
            balance: Vec::with_capacity(3),
            nonce: Vec::with_capacity(5),
            storage: Vec::with_capacity(7),
            submitted: Vec::with_capacity(11),
            ..Default::default()
        };
        conditions.block.excluded.reserve_exact(4);
        conditions.flashblock.excluded.reserve_exact(6);
        conditions.block.insert(ValidityOperator::NotEqual, U256::ONE);
        conditions.flashblock.insert(ValidityOperator::NotEqual, U256::ONE);
        assert_eq!(
            conditions.heap_size(),
            26 * core::mem::size_of::<ValidityPredicate>() + 10 * core::mem::size_of::<U256>()
        );

        let mut submitted = Vec::with_capacity(13);
        submitted.push(ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThan,
            value: U256::ZERO,
        });
        let conditions = ValidityConditions::new(submitted);
        assert_eq!(conditions.heap_size(), 13 * core::mem::size_of::<ValidityPredicate>());
        assert_eq!(conditions.len(), 1);
    }

    #[test]
    fn normalized_iteration_borrows_state_and_preserves_membership() {
        let block = ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThan,
            value: U256::from(9),
        };
        let normalized_block = ValidityPredicate::BlockNumber {
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(10),
        };
        let flashblock = ValidityPredicate::FlashblockIndex {
            op: ValidityOperator::NotEqual,
            value: U256::from(2),
        };
        let balance = ValidityPredicate::Balance {
            address: Address::ZERO,
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };
        let nonce = ValidityPredicate::Nonce {
            address: Address::ZERO,
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };
        let storage = ValidityPredicate::Storage {
            address: Address::ZERO,
            slot: U256::ZERO,
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };
        let raw = vec![
            storage.clone(),
            block.clone(),
            nonce.clone(),
            flashblock.clone(),
            balance.clone(),
        ];
        let conditions = ValidityConditions::new(raw.clone());
        let normalized = conditions.iter().collect::<Vec<_>>();
        assert_eq!(
            normalized,
            vec![
                Cow::Owned(normalized_block.clone()),
                Cow::Owned(flashblock.clone()),
                Cow::Borrowed(&balance),
                Cow::Borrowed(&nonce),
                Cow::Borrowed(&storage),
            ]
        );
        assert!(normalized[2..].iter().all(|predicate| matches!(predicate, Cow::Borrowed(_))));
        for predicate in [&normalized_block, &flashblock, &balance, &nonce, &storage] {
            assert!(conditions.contains(predicate));
        }
        assert!(!conditions.contains(&block));
        assert!(!conditions.contains(&ValidityPredicate::Nonce {
            address: Address::ZERO,
            op: ValidityOperator::Equal,
            value: U256::from(1),
        }));
        assert_eq!(conditions.submitted(), raw);
        assert_eq!(conditions.len(), raw.len());
    }

    #[test]
    fn normalized_timing_conjunctions_preserve_matching_and_expiry() {
        let operators = [
            ValidityOperator::LessThan,
            ValidityOperator::LessThanOrEqual,
            ValidityOperator::Equal,
            ValidityOperator::NotEqual,
            ValidityOperator::GreaterThan,
            ValidityOperator::GreaterThanOrEqual,
        ];
        let values = [
            U256::ZERO,
            U256::from(1),
            U256::from(2),
            U256::from(u64::MAX),
            U256::from(u64::MAX) + U256::from(1),
            U256::MAX,
        ];
        let mut db = InMemoryDB::default();
        for flashblock in [false, true] {
            for first in operators {
                for second in operators {
                    for left in values {
                        for right in values {
                            let predicate = |op, value| {
                                if flashblock {
                                    ValidityPredicate::FlashblockIndex { op, value }
                                } else {
                                    ValidityPredicate::BlockNumber { op, value }
                                }
                            };
                            let raw = [predicate(first, left), predicate(second, right)];
                            let conditions = ValidityConditions::new(raw.to_vec());
                            assert_eq!(
                                conditions.block().expiry_bound(),
                                ValidityPredicate::block_expiry_bound(&raw)
                            );
                            assert_eq!(
                                conditions.flashblock().expiry_bound(),
                                ValidityPredicate::flashblock_expiry_bound(&raw)
                            );
                            for position in [0, 1, 2, 3, u64::MAX] {
                                let context = PredicateContext {
                                    block_number: position,
                                    flashblock_index: position,
                                };
                                let expected = raw
                                    .iter()
                                    .all(|predicate| predicate.matches(&mut db, &context).unwrap());
                                let actual = conditions
                                    .iter()
                                    .all(|predicate| predicate.matches(&mut db, &context).unwrap());
                                assert_eq!(actual, expected, "{raw:?} at {context:?}");
                                let direct = conditions
                                    .block()
                                    .first_unsatisfied(U256::from(position))
                                    .is_none()
                                    && conditions
                                        .flashblock()
                                        .first_unsatisfied(U256::from(position))
                                        .is_none();
                                assert_eq!(direct, expected, "compiled {raw:?} at {context:?}");
                                assert_eq!(
                                    conditions.is_expired(&context),
                                    ValidityPredicate::is_batch_expired(&raw, &context)
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn tightest_bounds_keep_exclusions_and_submission_count() {
        let raw = vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThan,
                value: U256::from(9),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(8),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThan,
                value: U256::from(21),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(30),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::NotEqual,
                value: U256::from(15),
            },
        ];
        let conditions = ValidityConditions::new(raw.clone());
        assert_eq!(conditions.submitted(), raw);
        assert_eq!(conditions.block().min(), Some(U256::from(10)));
        assert_eq!(conditions.block().max(), Some(U256::from(20)));
        assert_eq!(conditions.len(), raw.len());
        assert!(!conditions.is_empty());
        let mut db = InMemoryDB::default();
        for (block_number, expected) in
            [(9, false), (10, true), (15, false), (20, true), (21, false)]
        {
            let context = PredicateContext { block_number, flashblock_index: 1 };
            assert_eq!(
                conditions.iter().all(|predicate| predicate.matches(&mut db, &context).unwrap()),
                expected
            );
        }
    }

    #[test]
    fn flashblock_expiry_resets_until_last_allowed_block() {
        let conditions = ValidityConditions::new(vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(102),
            },
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThan,
                value: U256::from(3),
            },
        ]);
        for (block_number, flashblock_index, expired) in
            [(100, 3, false), (101, 1, false), (102, 2, false), (102, 3, true), (103, 1, true)]
        {
            let context = PredicateContext { block_number, flashblock_index };
            assert_eq!(conditions.is_expired(&context), expired);
        }
    }

    #[test]
    fn state_predicates_remain_enforced_and_do_not_establish_expiry() {
        let address = Address::ZERO;
        let balance =
            ValidityPredicate::Balance { address, op: ValidityOperator::Equal, value: U256::ZERO };
        let nonce =
            ValidityPredicate::Nonce { address, op: ValidityOperator::Equal, value: U256::ZERO };
        let storage = ValidityPredicate::Storage {
            address,
            slot: U256::ZERO,
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(1),
        };
        let conditions =
            ValidityConditions::new(vec![storage.clone(), nonce.clone(), balance.clone()]);
        let context = PredicateContext { block_number: 100, flashblock_index: 1 };
        let mut db = InMemoryDB::default();
        assert!(conditions.contains(&balance));
        assert!(conditions.contains(&nonce));
        assert!(conditions.contains(&storage));
        assert!(!conditions.iter().all(|predicate| predicate.matches(&mut db, &context).unwrap()));
        assert!(!conditions.is_expired(&context));
        assert_eq!(conditions.block().expiry_bound(), None);
    }

    #[test]
    fn explicit_unbounded_flashblock_comparisons_are_still_present() {
        let mut bounds = ValidityBounds::default();
        bounds.insert(ValidityOperator::GreaterThanOrEqual, U256::ZERO);
        assert!(bounds.is_present());
        assert_eq!(bounds.expiry_bound(), None);
    }
}
