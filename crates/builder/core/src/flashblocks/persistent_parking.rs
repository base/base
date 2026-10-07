//! Validity-predicate parking that persists across flashblocks and blocks (prototype).

use alloy_primitives::{TxHash, map::B256Set};
use base_execution_payload_builder::ParkedPredicateIndex;
use base_execution_txpool::{BasePooledTx, PredicateContext, ValidityPredicate};
use revm::Database;

/// Parked validity transactions kept across flashblock and block builds.
///
/// Membership means the transaction's recorded blocking predicate is false for the current
/// build state and context, and its predicate batch is not expired. The flashblock loop skips
/// members without evaluating them, which yields the same inclusion as re-evaluating them,
/// provided every state or context change that could falsify membership goes through
/// [`ParkedPredicateIndex::affected_by_state`], [`Self::begin_flashblock`] or
/// [`Self::begin_block`].
#[derive(Debug)]
pub struct PersistentValidityParking<T> {
    /// Parked transactions indexed by their blocking predicate.
    pub index: ParkedPredicateIndex<T>,
    /// Members whose predicates reference the flashblock index, which changes every flashblock.
    flashblock_sensitive: B256Set,
}

impl<T: BasePooledTx> PersistentValidityParking<T> {
    /// Creates empty parking whose state buckets convert at `ordered_threshold` entries.
    pub fn new(ordered_threshold: usize) -> Self {
        Self {
            index: ParkedPredicateIndex::new(ordered_threshold),
            flashblock_sensitive: B256Set::default(),
        }
    }

    /// Parks a transaction under its currently unsatisfied `predicate`.
    pub fn park(&mut self, transaction_hash: TxHash, transaction: T, predicate: ValidityPredicate) {
        if transaction
            .validity_predicates()
            .iter()
            .any(|predicate| matches!(predicate, ValidityPredicate::FlashblockIndex { .. }))
        {
            self.flashblock_sensitive.insert(transaction_hash);
        }
        self.index.park(transaction_hash, transaction, predicate);
    }

    /// Releases members whose outcome can change with the flashblock index, so the next
    /// flashblock evaluates them as fresh candidates. Returns the number released.
    pub fn begin_flashblock(&mut self) -> usize {
        let mut released = 0;
        for hash in self.flashblock_sensitive.drain() {
            released += usize::from(self.index.remove(hash).is_some());
        }
        released
    }

    /// Revalidates every member against the state and context a new block starts from, after
    /// pre-execution changes and sequencer transactions. Members that left the pool, whose
    /// blocking predicate now holds or cannot be read, or whose batch expired are released.
    /// Returns the number released.
    pub fn begin_block<DB: Database>(
        &mut self,
        db: &mut DB,
        context: &PredicateContext,
        mut is_pooled: impl FnMut(&TxHash) -> bool,
    ) -> usize {
        let released = self.index.retain(|hash, transaction, predicate| {
            is_pooled(hash)
                && matches!(predicate.matches(db, context), Ok(false))
                && !ValidityPredicate::is_batch_expired(transaction.validity_predicates(), context)
        });
        let index = &self.index;
        self.flashblock_sensitive.retain(|hash| index.contains(*hash));
        released
    }

    /// Releases every member, for a parent this builder did not build.
    pub fn clear(&mut self) {
        self.index.retain(|_, _, _| false);
        self.flashblock_sensitive.clear();
    }
}
