//! State predicates found unsatisfied when a validity transaction was admitted to the pool.

use alloy_primitives::B256;
use revm::Database;

use crate::{PredicateContext, ValidityConditions, ValidityPredicate};

/// The first state predicate of a validity transaction that was unsatisfied at pool admission,
/// and the canonical block whose post-state it was evaluated against.
///
/// Pool validation runs on the pool's validation tasks, off the build thread. Recording the result
/// there lets a builder working on top of `state_block_hash` rest the transaction without reading
/// predicate state itself, as long as nothing in the block being built has yet written the state
/// the predicate reads. Only state predicates are recorded: block and flashblock predicates read no
/// state and cost the build loop nothing to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestionRest {
    /// Hash of the canonical block whose post-state the predicate was evaluated against.
    pub state_block_hash: B256,
    /// The first unsatisfied state predicate, in the order the build loop evaluates them.
    pub predicate: ValidityPredicate,
}

impl IngestionRest {
    /// Evaluates the state predicates of `conditions` against `db`, the post-state of
    /// `state_block_hash`, in the order the build loop evaluates them.
    ///
    /// Returns `None` when every state predicate is satisfied.
    pub fn evaluate<DB: Database>(
        conditions: &ValidityConditions,
        db: &mut DB,
        state_block_hash: B256,
    ) -> Result<Option<Self>, DB::Error> {
        // State predicates do not read the build position.
        let context = PredicateContext { block_number: 0, flashblock_index: 0 };
        for predicate in conditions.state_predicates() {
            if !predicate.matches(db, &context)? {
                return Ok(Some(Self { state_block_hash, predicate: predicate.clone() }));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use revm::{database::InMemoryDB, state::AccountInfo};

    use super::*;
    use crate::ValidityOperator;

    const WATCHED: Address = Address::repeat_byte(0xaa);
    const BLOCK: B256 = B256::repeat_byte(0x01);

    fn balance_at_least(value: u64) -> ValidityPredicate {
        ValidityPredicate::Balance {
            address: WATCHED,
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(value),
        }
    }

    fn nonce_equals(value: u64) -> ValidityPredicate {
        ValidityPredicate::Nonce {
            address: WATCHED,
            op: ValidityOperator::Equal,
            value: U256::from(value),
        }
    }

    fn db_with_balance(balance: u64) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(WATCHED, AccountInfo::from_balance(U256::from(balance)));
        db
    }

    #[test]
    fn records_the_first_unsatisfied_state_predicate() {
        let conditions = ValidityConditions::new(vec![
            balance_at_least(5),
            nonce_equals(7),
            balance_at_least(100),
        ]);

        let rest = IngestionRest::evaluate(&conditions, &mut db_with_balance(10), BLOCK).unwrap();

        // Balance 10 satisfies `>= 5`, fails `>= 100`; balance predicates are evaluated before
        // nonce predicates, so the second balance predicate is the first unsatisfied one.
        assert_eq!(
            rest,
            Some(IngestionRest { state_block_hash: BLOCK, predicate: balance_at_least(100) })
        );
    }

    #[test]
    fn satisfied_state_predicates_record_nothing() {
        let conditions = ValidityConditions::new(vec![balance_at_least(5), nonce_equals(0)]);

        assert_eq!(
            IngestionRest::evaluate(&conditions, &mut db_with_balance(10), BLOCK).unwrap(),
            None
        );
    }

    #[test]
    fn timing_predicates_are_not_evaluated() {
        // An unsatisfiable block bound is left to the build loop, which reads no state for it.
        let conditions = ValidityConditions::new(vec![ValidityPredicate::BlockNumber {
            op: ValidityOperator::Equal,
            value: U256::MAX,
        }]);

        assert_eq!(
            IngestionRest::evaluate(&conditions, &mut db_with_balance(0), BLOCK).unwrap(),
            None
        );
    }
}
