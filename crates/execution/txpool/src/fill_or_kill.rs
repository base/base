//! Pool-side expiry of fill-immediately-or-kill validity transactions.
//!
//! A flagged transaction must land in the first block in which its predicates hold. The builder
//! enforces that within the block it builds (see the payload builder's predicate write journal),
//! but its decisions live in one process: after a sequencer failover, or on a mempool node that
//! keeps re-forwarding the transaction, a killed transaction would be revived because its loose
//! predicates still hold. This module derives the same decision from canonical state alone, so
//! every pool reaches it independently.
//!
//! When block `B` becomes canonical, a flagged transaction that has been pending since before
//! block `B` started building is evicted if its predicates held at the start of `B` (it should
//! have been filled in `B` or was killed there) or hold at the end of `B` (it was satisfiable in
//! `B` but did not land). A transaction admitted while `B` was building is given `B + 1`: it may
//! not have reached the builder in time to be considered.

use std::collections::HashMap;

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, TxHash, U256, map::B256Set};
use base_common_consensus::BasePrimitives;
use futures::StreamExt;
use reth_provider::{CanonStateNotification, StateProviderFactory};
use reth_storage_api::{StateProvider, errors::provider::ProviderError};
use revm::{
    Database,
    state::{AccountInfo, Bytecode},
};
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{debug, warn};

use crate::{PredicateContext, ValidityPredicate};

/// Whether a flagged transaction's predicates hold against one canonical block's final state.
///
/// Block-number predicates are evaluated for both the block that just closed and the one about to
/// be built; state predicates read the same state either way. Flashblock-index predicates only
/// restrict where in a block a transaction may land, so they count as satisfied here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Satisfiability {
    /// The predicates held at the end of the committed block, as a transaction of that block.
    pub at_block_end: bool,
    /// The predicates hold at the start of the next block, as a transaction of that block.
    pub at_next_block_start: bool,
}

impl Satisfiability {
    /// Evaluates `predicates` against `db`, the final state of `committed_block`.
    pub fn evaluate<DB: Database>(
        predicates: &[ValidityPredicate],
        db: &mut DB,
        committed_block: u64,
    ) -> Result<Self, DB::Error> {
        let committed = PredicateContext { block_number: committed_block, flashblock_index: 0 };
        let next = PredicateContext {
            block_number: committed_block.saturating_add(1),
            flashblock_index: 0,
        };
        let mut satisfiability = Self { at_block_end: true, at_next_block_start: true };
        for predicate in predicates {
            match predicate {
                ValidityPredicate::FlashblockIndex { .. } => {}
                ValidityPredicate::BlockNumber { .. } => {
                    satisfiability.at_block_end &= predicate.matches(db, &committed)?;
                    satisfiability.at_next_block_start &= predicate.matches(db, &next)?;
                }
                _ => {
                    let holds = predicate.matches(db, &committed)?;
                    satisfiability.at_block_end &= holds;
                    satisfiability.at_next_block_start &= holds;
                }
            }
            if !satisfiability.at_block_end && !satisfiability.at_next_block_start {
                break;
            }
        }
        Ok(satisfiability)
    }
}

/// A flagged transaction tracked by the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FillOrKillEntry {
    /// Canonical head when the transaction was admitted.
    admitted_head: u64,
    /// Whether the predicates held at the start of the block being built after the last
    /// canonical block this entry was evaluated at.
    satisfiable_at_next_block_start: bool,
}

/// Fill-immediately-or-kill transactions pending in the pool, with the canonical head they were
/// admitted at and their satisfiability at the latest evaluated block.
#[derive(Debug, Default)]
pub struct FillOrKillIndex {
    entries: HashMap<TxHash, FillOrKillEntry>,
}

impl FillOrKillIndex {
    /// Tracks `hash`, admitted while `admitted_head` was the canonical head.
    pub fn insert(&mut self, hash: TxHash, admitted_head: u64) {
        self.entries.insert(
            hash,
            FillOrKillEntry { admitted_head, satisfiable_at_next_block_start: false },
        );
    }

    /// Stops tracking `hash`.
    pub fn remove(&mut self, hash: &TxHash) {
        self.entries.remove(hash);
    }

    /// Returns whether `hash` is tracked.
    pub fn contains(&self, hash: &TxHash) -> bool {
        self.entries.contains_key(hash)
    }

    /// Returns the number of tracked transactions.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether no transaction is tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Applies canonical block `committed_block` and returns the hashes to evict.
    ///
    /// `evaluate` returns a transaction's [`Satisfiability`] against the block's final state, or
    /// `None` if the pool no longer holds it, in which case it is dropped from the index. A
    /// transaction that has been pending since before the block started building is evicted if
    /// its predicates held at the block's start (remembered from the previous block) or hold at
    /// its end. Every remaining entry remembers whether it is satisfiable at the start of the
    /// next block.
    pub fn apply_canonical_block(
        &mut self,
        committed_block: u64,
        mut evaluate: impl FnMut(&TxHash) -> Option<Satisfiability>,
    ) -> Vec<TxHash> {
        let mut evicted = Vec::new();
        self.entries.retain(|hash, entry| {
            let Some(satisfiability) = evaluate(hash) else {
                return false;
            };
            // Admitted at head `H`, the transaction was pending for the whole build of block
            // `H + 2` and later. Block `H + 1` started building as soon as `H` was canonical.
            let pending_for_whole_block = entry.admitted_head.saturating_add(2) <= committed_block;
            if pending_for_whole_block
                && (entry.satisfiable_at_next_block_start || satisfiability.at_block_end)
            {
                evicted.push(*hash);
                return false;
            }
            entry.satisfiable_at_next_block_start = satisfiability.at_next_block_start;
            true
        });
        evicted
    }
}

/// A read-only [`Database`] over a canonical [`StateProvider`], for predicate evaluation.
pub struct StateProviderPredicateDb<'a> {
    state: &'a dyn StateProvider,
}

impl<'a> StateProviderPredicateDb<'a> {
    /// Reads predicate state from `state`.
    pub const fn new(state: &'a dyn StateProvider) -> Self {
        Self { state }
    }
}

impl std::fmt::Debug for StateProviderPredicateDb<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateProviderPredicateDb").finish_non_exhaustive()
    }
}

impl Database for StateProviderPredicateDb<'_> {
    type Error = ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.state.basic_account(&address)?.map(|account| AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.get_bytecode_hash(),
            ..AccountInfo::default()
        }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self.state.bytecode_by_hash(&code_hash)?.map(|bytecode| bytecode.0).unwrap_or_default())
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.state.storage(address, B256::from(index))?.unwrap_or_default())
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.state.block_hash(number)?.unwrap_or_default())
    }
}

/// Evicts fill-immediately-or-kill transactions from a pool as canonical blocks commit.
pub trait FillOrKillExpiry: Clone + Send + Sync + 'static {
    /// Applies canonical block `committed_block`, whose final state is `state` and whose
    /// transactions are `mined`, and returns the number of flagged transactions evicted.
    fn expire_fill_or_kill(
        &self,
        committed_block: u64,
        mined: &B256Set,
        state: &dyn StateProvider,
    ) -> usize;
}

/// Feeds each canonical block into the pool's fill-immediately-or-kill expiry.
pub async fn maintain_fill_or_kill_expiry<P, Provider>(
    pool: P,
    provider: Provider,
    mut events: BroadcastStream<CanonStateNotification<BasePrimitives>>,
) where
    P: FillOrKillExpiry,
    Provider: StateProviderFactory,
{
    loop {
        let notification = match events.next().await {
            Some(Ok(notification)) => notification,
            Some(Err(BroadcastStreamRecvError::Lagged(missed))) => {
                // Skipped blocks are caught up at the next one: a transaction satisfiable in a
                // skipped block is still evaluated against the next block's state.
                warn!(missed, "canonical state stream lagged; fill-or-kill expiry skipped blocks");
                continue;
            }
            None => break,
        };
        let committed = notification.committed();
        let tip = committed.tip();
        let block_number = tip.header().number();
        let state = match provider.state_by_block_hash(tip.hash()) {
            Ok(state) => state,
            Err(error) => {
                warn!(
                    block = block_number,
                    error = %error,
                    "fill-or-kill expiry could not open the canonical block state"
                );
                continue;
            }
        };
        let mined = committed
            .blocks_iter()
            .flat_map(|block| block.body().transactions().map(|tx| B256::from(*tx.tx_hash())))
            .collect::<B256Set>();
        let evicted = pool.expire_fill_or_kill(block_number, &mined, &*state);
        if evicted > 0 {
            debug!(
                block = block_number,
                evicted,
                "evicted fill-immediately-or-kill transactions satisfiable in a block they missed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use revm::database::InMemoryDB;

    use super::*;
    use crate::ValidityOperator;

    fn hash(byte: u8) -> TxHash {
        TxHash::repeat_byte(byte)
    }

    fn satisfied(at_block_end: bool, at_next_block_start: bool) -> Satisfiability {
        Satisfiability { at_block_end, at_next_block_start }
    }

    #[test]
    fn evaluates_block_bounds_for_the_committed_and_the_next_block() {
        let watched = Address::repeat_byte(0x11);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            watched,
            AccountInfo { balance: U256::from(5), ..Default::default() },
        );
        let predicates = [
            ValidityPredicate::Balance {
                address: watched,
                op: ValidityOperator::GreaterThan,
                value: U256::from(1),
            },
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(100),
            },
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(1),
            },
        ];

        assert_eq!(
            Satisfiability::evaluate(&predicates, &mut db, 99).unwrap(),
            satisfied(true, true)
        );
        assert_eq!(
            Satisfiability::evaluate(&predicates, &mut db, 100).unwrap(),
            satisfied(true, false)
        );
        assert_eq!(
            Satisfiability::evaluate(&predicates, &mut db, 101).unwrap(),
            satisfied(false, false)
        );
    }

    #[test]
    fn a_false_state_predicate_fails_both_positions() {
        let watched = Address::repeat_byte(0x11);
        let predicates = [ValidityPredicate::Balance {
            address: watched,
            op: ValidityOperator::GreaterThan,
            value: U256::from(1),
        }];

        assert_eq!(
            Satisfiability::evaluate(&predicates, &mut InMemoryDB::default(), 7).unwrap(),
            satisfied(false, false)
        );
    }

    #[test]
    fn evicts_once_satisfiable_in_a_block_it_was_pending_for() {
        let mut index = FillOrKillIndex::default();
        index.insert(hash(1), 10);

        // Block 11 was already building when the transaction arrived: not evicted even though
        // it was satisfiable there, but its satisfiability at the start of block 12 is kept.
        assert!(index.apply_canonical_block(11, |_| Some(satisfied(true, true))).is_empty());
        // Block 12: held at its start, so it should have landed or been killed there.
        assert_eq!(index.apply_canonical_block(12, |_| Some(satisfied(false, false))), [hash(1)]);
        assert!(index.is_empty());
    }

    #[test]
    fn evicts_when_satisfiable_at_the_end_of_a_block_it_missed() {
        let mut index = FillOrKillIndex::default();
        index.insert(hash(1), 10);

        assert!(index.apply_canonical_block(11, |_| Some(satisfied(false, false))).is_empty());
        assert_eq!(index.apply_canonical_block(12, |_| Some(satisfied(true, false))), [hash(1)]);
    }

    #[test]
    fn keeps_waiting_while_the_predicates_never_hold() {
        let mut index = FillOrKillIndex::default();
        index.insert(hash(1), 10);

        for block in 11..20 {
            assert!(
                index.apply_canonical_block(block, |_| Some(satisfied(false, false))).is_empty()
            );
        }
        assert!(index.contains(&hash(1)));
    }

    #[test]
    fn drops_entries_the_pool_no_longer_holds() {
        let mut index = FillOrKillIndex::default();
        index.insert(hash(1), 10);
        index.insert(hash(2), 10);

        let evicted = index.apply_canonical_block(12, |hash| {
            (*hash == self::hash(2)).then_some(satisfied(false, false))
        });

        assert!(evicted.is_empty());
        assert!(!index.contains(&hash(1)));
        assert!(index.contains(&hash(2)));
    }
}
