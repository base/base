//! Validity transactions known to be unsatisfied at a canonical head.
//!
//! Most validity transactions on mainnet are fill-or-kill orders whose predicate stays false
//! for the transaction's whole lifetime, so evaluating every one of them in every flashblock
//! repeats the same state reads. [`RestingPredicates`] classifies pool transactions once per
//! canonical block against the head state. A payload job built on that head then skips
//! evaluation for a resting transaction until the job's own execution changes the state its
//! blocking predicate reads, tracked by [`RestingPredicateView`].

use std::{sync::Arc, time::Instant};

use alloy_eips::BlockNumHash;
use alloy_primitives::{
    B256, TxHash,
    map::{B256Map, HashSet},
};
use base_common_consensus::BasePrimitives;
use base_execution_txpool::{
    BasePooledTx, FIRST_POOL_FLASHBLOCK_INDEX, PredicateContext, ValidityPredicate,
};
use futures::{Stream, StreamExt};
use parking_lot::RwLock;
use reth_chain_state::CanonStateNotification;
use reth_provider::{ProviderResult, StateProviderFactory};
use reth_revm::database::StateProviderDatabase;
use reth_transaction_pool::TransactionPool;
use revm::{
    Database,
    database::{CacheDB, TransitionState},
    state::EvmState,
};
use tracing::{debug, warn};

use crate::{BuilderMetrics, ValidityPredicateKey};

/// Validity transactions whose predicates were unsatisfied at one canonical head.
///
/// A transaction rests under the first predicate, in batch order, that is false against the
/// head state for the next block. Its batch cannot hold in a block built on that head until the
/// block's own execution changes the state that predicate reads. Flashblock-index predicates
/// never block a resting transaction because they change within a block, and expired batches
/// are left out so the builder still sees and evicts them.
#[derive(Debug, Default)]
pub struct RestingPredicates {
    head: B256,
    blockers: B256Map<ValidityPredicate>,
    watched: HashSet<ValidityPredicateKey>,
}

impl RestingPredicates {
    /// Classifies `transactions` against `db`, the state at canonical block `head`.
    ///
    /// Transactions whose predicates cannot be read are left out, so the builder evaluates them
    /// as usual.
    pub fn classify<'a, DB: Database>(
        head: B256,
        head_number: u64,
        transactions: impl IntoIterator<Item = (TxHash, &'a [ValidityPredicate])>,
        db: &mut DB,
    ) -> Self {
        let context = PredicateContext {
            block_number: head_number.saturating_add(1),
            flashblock_index: FIRST_POOL_FLASHBLOCK_INDEX,
        };
        let mut resting = Self { head, ..Default::default() };
        for (hash, predicates) in transactions {
            if ValidityPredicate::is_batch_expired(predicates, &context) {
                continue;
            }
            for predicate in predicates
                .iter()
                .filter(|predicate| !matches!(predicate, ValidityPredicate::FlashblockIndex { .. }))
            {
                match predicate.matches(db, &context) {
                    Ok(true) => continue,
                    Ok(false) => {
                        resting.watched.insert(ValidityPredicateKey::for_predicate(predicate));
                        resting.blockers.insert(hash, predicate.clone());
                    }
                    Err(error) => debug!(
                        tx_hash = %hash,
                        error = %error,
                        "failed to read validity predicate state while classifying"
                    ),
                }
                break;
            }
        }
        resting
    }

    /// Returns the number of resting transactions.
    pub fn len(&self) -> usize {
        self.blockers.len()
    }

    /// Returns whether no transaction is resting.
    pub fn is_empty(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// Shared handle to the latest [`RestingPredicates`] snapshot.
///
/// The builder service replaces the snapshot on every canonical block, and payload jobs read it
/// once per flashblock until they find one classified at their parent.
#[derive(Debug, Clone, Default)]
pub struct RestingPredicateStore {
    latest: Arc<RwLock<Option<Arc<RestingPredicates>>>>,
}

impl RestingPredicateStore {
    /// Replaces the latest snapshot.
    pub fn publish(&self, snapshot: RestingPredicates) {
        BuilderMetrics::resting_predicate_transactions().set(snapshot.len() as f64);
        *self.latest.write() = Some(Arc::new(snapshot));
    }

    /// Returns a view of the latest snapshot when it was classified at `parent`.
    pub fn view_for_parent(&self, parent: B256) -> Option<RestingPredicateView> {
        let snapshot = self.latest.read().clone()?;
        RestingPredicateView::for_parent(snapshot, parent)
    }

    /// Classifies pending pool transactions at every new canonical tip until `notifications`
    /// ends.
    ///
    /// Each tip is classified from scratch, so a reorg needs no special handling: the next
    /// snapshot reflects the new head, and jobs building on any other parent ignore it.
    pub async fn maintain<Client, Pool, Notifications>(
        self,
        client: Client,
        pool: Pool,
        mut notifications: Notifications,
    ) where
        Client: StateProviderFactory + Clone + 'static,
        Pool: TransactionPool<Transaction: BasePooledTx> + 'static,
        Notifications: Stream<Item = CanonStateNotification<BasePrimitives>> + Unpin,
    {
        while let Some(notification) = notifications.next().await {
            let tip = notification.tip().num_hash();
            let client = client.clone();
            let pool = pool.clone();
            match tokio::task::spawn_blocking(move || Self::classify_tip(&client, &pool, tip)).await
            {
                Ok(Ok(snapshot)) => self.publish(snapshot),
                Ok(Err(error)) => {
                    warn!(block = %tip.number, error = %error, "failed to classify resting validity transactions");
                }
                Err(error) => {
                    warn!(block = %tip.number, error = %error, "resting validity classification task failed");
                }
            }
        }
    }

    fn classify_tip<Client, Pool>(
        client: &Client,
        pool: &Pool,
        tip: BlockNumHash,
    ) -> ProviderResult<RestingPredicates>
    where
        Client: StateProviderFactory,
        Pool: TransactionPool<Transaction: BasePooledTx>,
    {
        let started = Instant::now();
        let mut db =
            CacheDB::new(StateProviderDatabase::new(client.state_by_block_hash(tip.hash)?));
        let pending = pool.pending_transactions();
        let snapshot = RestingPredicates::classify(
            tip.hash,
            tip.number,
            pending.iter().map(|tx| (*tx.hash(), tx.transaction.validity_predicates())),
            &mut db,
        );
        BuilderMetrics::resting_predicate_classification_duration().record(started.elapsed());
        Ok(snapshot)
    }
}

/// A payload job's view of a [`RestingPredicates`] snapshot taken at the job's parent block.
///
/// Tracks which watched keys the job has changed since the parent. A resting transaction whose
/// blocking key was changed is evaluated as usual; every other resting transaction is still
/// unsatisfied at the current build position.
#[derive(Debug)]
pub struct RestingPredicateView {
    snapshot: Arc<RestingPredicates>,
    touched: HashSet<ValidityPredicateKey>,
}

impl RestingPredicateView {
    /// Creates a view when `snapshot` was classified at `parent`, the block being built on.
    pub fn for_parent(snapshot: Arc<RestingPredicates>, parent: B256) -> Option<Self> {
        (snapshot.head == parent).then(|| Self { snapshot, touched: HashSet::default() })
    }

    /// Recomputes the touched keys from `transitions`, the job's accumulated changes since the
    /// parent block.
    ///
    /// Iterates watched keys rather than the transitions, so the cost tracks the resting set and
    /// not the size of the block. A value changed and then restored counts as untouched, since
    /// the predicate reads the same value it was classified against.
    pub fn sync_transitions(&mut self, transitions: Option<&TransitionState>) {
        self.touched.clear();
        let Some(transitions) = transitions else { return };
        for key in &self.snapshot.watched {
            let changed = match key {
                ValidityPredicateKey::Balance(address) => transitions
                    .transitions
                    .get(address)
                    .is_some_and(|account| account.current_balance() != account.previous_balance()),
                ValidityPredicateKey::Storage(address, slot) => transitions
                    .transitions
                    .get(address)
                    .and_then(|account| account.storage.get(slot))
                    .is_some_and(|value| value.is_changed()),
                ValidityPredicateKey::BlockNumber | ValidityPredicateKey::FlashblockIndex => false,
            };
            if changed {
                self.touched.insert(*key);
            }
        }
    }

    /// Records watched keys changed by one committed transaction.
    pub fn record_state(&mut self, state: &EvmState) {
        for (address, account) in state {
            if account.info.balance != account.original_info().balance {
                self.touch(ValidityPredicateKey::Balance(*address));
            }
            for (slot, _) in account.changed_storage_slots() {
                self.touch(ValidityPredicateKey::Storage(*address, *slot));
            }
        }
    }

    /// Returns the predicate a transaction rests under, or `None` when it must be evaluated.
    ///
    /// A transaction re-added under the same hash with a batch that no longer contains the
    /// classified predicate is evaluated as usual.
    pub fn blocker(
        &self,
        hash: TxHash,
        predicates: &[ValidityPredicate],
    ) -> Option<&ValidityPredicate> {
        self.snapshot.blockers.get(&hash).filter(|blocker| {
            predicates.contains(blocker)
                && !self.touched.contains(&ValidityPredicateKey::for_predicate(blocker))
        })
    }

    fn touch(&mut self, key: ValidityPredicateKey) {
        if self.snapshot.watched.contains(&key) {
            self.touched.insert(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::{Address, B256, U256};
    use base_execution_txpool::{ValidityOperator, ValidityPredicate};
    use revm::{
        database::{
            AccountStatus, InMemoryDB, TransitionAccount, TransitionState, states::StorageSlot,
        },
        primitives::HashMap,
        state::{Account, AccountInfo, EvmState, EvmStorageSlot},
    };

    use super::{RestingPredicateView, RestingPredicates};

    const HEAD: B256 = B256::repeat_byte(0xaa);
    const HEAD_NUMBER: u64 = 100;
    const SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

    fn balance_at_least(address: Address, value: u64) -> ValidityPredicate {
        ValidityPredicate::Balance {
            address,
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(value),
        }
    }

    fn storage_equals(address: Address, value: u64) -> ValidityPredicate {
        ValidityPredicate::Storage {
            address,
            slot: SLOT,
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(value),
        }
    }

    fn block_number(op: ValidityOperator, value: u64) -> ValidityPredicate {
        ValidityPredicate::BlockNumber { op, value: U256::from(value) }
    }

    fn flashblock_index(op: ValidityOperator, value: u64) -> ValidityPredicate {
        ValidityPredicate::FlashblockIndex { op, value: U256::from(value) }
    }

    fn hash(byte: u8) -> B256 {
        B256::with_last_byte(byte)
    }

    fn classify(transactions: &[(B256, Vec<ValidityPredicate>)]) -> RestingPredicates {
        RestingPredicates::classify(
            HEAD,
            HEAD_NUMBER,
            transactions.iter().map(|(hash, predicates)| (*hash, predicates.as_slice())),
            &mut InMemoryDB::default(),
        )
    }

    fn view(transactions: &[(B256, Vec<ValidityPredicate>)]) -> RestingPredicateView {
        RestingPredicateView::for_parent(Arc::new(classify(transactions)), HEAD)
            .expect("snapshot was classified at the parent")
    }

    fn balance_change(address: Address, old: u64, new: u64) -> EvmState {
        let mut account = Account::default();
        account.info.balance = U256::from(new);
        account.original_info_mut().balance = U256::from(old);
        EvmState::from_iter([(address, account)])
    }

    fn storage_change(address: Address, old: u64, new: u64) -> EvmState {
        let mut account = Account::default();
        account.storage.insert(
            SLOT,
            EvmStorageSlot::new_changed(U256::from(old), U256::from(new), Default::default()),
        );
        EvmState::from_iter([(address, account)])
    }

    fn transitions(address: Address, account: TransitionAccount) -> TransitionState {
        TransitionState { transitions: HashMap::from_iter([(address, account)]) }
    }

    fn slot_transition(address: Address, original: u64, present: u64) -> TransitionState {
        let account = TransitionAccount {
            info: Some(AccountInfo::default()),
            previous_info: Some(AccountInfo::default()),
            storage: HashMap::from_iter([(
                SLOT,
                StorageSlot::new_changed(U256::from(original), U256::from(present)),
            )]),
            status: AccountStatus::Changed,
            ..Default::default()
        };
        transitions(address, account)
    }

    #[test]
    fn rests_only_transactions_unsatisfied_at_the_head() {
        let funded = Address::with_last_byte(1);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            funded,
            AccountInfo { balance: U256::from(10), ..Default::default() },
        );

        let resting = RestingPredicates::classify(
            HEAD,
            HEAD_NUMBER,
            [
                (hash(1), [balance_at_least(funded, 10)].as_slice()),
                (hash(2), [balance_at_least(funded, 11)].as_slice()),
            ],
            &mut db,
        );

        let view = RestingPredicateView::for_parent(Arc::new(resting), HEAD).unwrap();
        assert!(view.blocker(hash(1), &[balance_at_least(funded, 10)]).is_none());
        assert_eq!(
            view.blocker(hash(2), &[balance_at_least(funded, 11)]),
            Some(&balance_at_least(funded, 11))
        );
    }

    #[test]
    fn rests_under_the_first_unsatisfied_predicate() {
        let address = Address::with_last_byte(1);
        let predicates = vec![
            block_number(ValidityOperator::LessThanOrEqual, HEAD_NUMBER + 5),
            storage_equals(address, 3),
            balance_at_least(address, 1),
        ];
        let view = view(&[(hash(1), predicates.clone())]);
        assert_eq!(view.blocker(hash(1), &predicates), Some(&predicates[1]));
    }

    #[test]
    fn view_requires_the_snapshot_head_to_be_the_parent() {
        let snapshot = Arc::new(classify(&[]));
        assert!(RestingPredicateView::for_parent(snapshot, B256::repeat_byte(0xbb)).is_none());
    }

    #[test]
    fn flashblock_index_predicates_never_block() {
        let predicates = vec![flashblock_index(ValidityOperator::GreaterThanOrEqual, 5)];
        let view = view(&[(hash(1), predicates.clone())]);
        assert!(view.blocker(hash(1), &predicates).is_none());
    }

    #[test]
    fn block_number_is_evaluated_for_the_next_block() {
        let next_block = HEAD_NUMBER + 1;
        let reachable = vec![block_number(ValidityOperator::GreaterThanOrEqual, next_block)];
        let later = vec![block_number(ValidityOperator::GreaterThanOrEqual, next_block + 1)];
        let view = view(&[(hash(1), reachable.clone()), (hash(2), later.clone())]);

        assert!(view.blocker(hash(1), &reachable).is_none());
        assert!(view.blocker(hash(2), &later).is_some());
    }

    #[test]
    fn lower_bound_stops_resting_once_the_next_block_reaches_it() {
        let predicates = vec![block_number(ValidityOperator::GreaterThanOrEqual, HEAD_NUMBER + 2)];
        let transactions = [(hash(1), predicates.clone())];
        assert!(view(&transactions).blocker(hash(1), &predicates).is_some());

        let next_head = B256::repeat_byte(0xbb);
        let next = RestingPredicates::classify(
            next_head,
            HEAD_NUMBER + 1,
            transactions.iter().map(|(hash, predicates)| (*hash, predicates.as_slice())),
            &mut InMemoryDB::default(),
        );
        let view = RestingPredicateView::for_parent(Arc::new(next), next_head).unwrap();
        assert!(view.blocker(hash(1), &predicates).is_none());
    }

    #[test]
    fn expired_batches_are_left_for_the_builder_to_evict() {
        let predicates = vec![
            block_number(ValidityOperator::LessThanOrEqual, HEAD_NUMBER),
            balance_at_least(Address::with_last_byte(1), 1),
        ];
        let view = view(&[(hash(1), predicates.clone())]);
        assert!(view.blocker(hash(1), &predicates).is_none());
    }

    #[test]
    fn changed_predicates_under_the_same_hash_are_not_resting() {
        let address = Address::with_last_byte(1);
        let view = view(&[(hash(1), vec![balance_at_least(address, 5)])]);
        assert!(view.blocker(hash(1), &[balance_at_least(address, 6)]).is_none());
    }

    #[test]
    fn committed_state_change_on_the_blocking_key_ends_resting() {
        let watched = Address::with_last_byte(1);
        let other = Address::with_last_byte(2);
        let balance = vec![balance_at_least(watched, 5)];
        let storage = vec![storage_equals(watched, 3)];
        let mut view = view(&[(hash(1), balance.clone()), (hash(2), storage.clone())]);

        view.record_state(&balance_change(other, 0, 9));
        view.record_state(&storage_change(other, 0, 3));
        assert!(view.blocker(hash(1), &balance).is_some());
        assert!(view.blocker(hash(2), &storage).is_some());

        view.record_state(&balance_change(watched, 0, 1));
        assert!(view.blocker(hash(1), &balance).is_none());
        assert!(view.blocker(hash(2), &storage).is_some());

        view.record_state(&storage_change(watched, 0, 1));
        assert!(view.blocker(hash(2), &storage).is_none());
    }

    #[test]
    fn earlier_flashblock_changes_end_resting_and_restored_values_do_not() {
        let watched = Address::with_last_byte(1);
        let storage = vec![storage_equals(watched, 3)];
        let mut view = view(&[(hash(1), storage.clone())]);

        view.sync_transitions(Some(&slot_transition(watched, 0, 0)));
        assert!(view.blocker(hash(1), &storage).is_some());

        view.sync_transitions(Some(&slot_transition(watched, 0, 1)));
        assert!(view.blocker(hash(1), &storage).is_none());

        view.sync_transitions(None);
        assert!(view.blocker(hash(1), &storage).is_some());
    }

    #[test]
    fn earlier_flashblock_balance_change_ends_resting() {
        let watched = Address::with_last_byte(1);
        let balance = vec![balance_at_least(watched, 5)];
        let mut view = view(&[(hash(1), balance.clone())]);

        let account = TransitionAccount {
            info: Some(AccountInfo { balance: U256::from(5), ..Default::default() }),
            status: AccountStatus::InMemoryChange,
            ..Default::default()
        };
        view.sync_transitions(Some(&transitions(watched, account)));
        assert!(view.blocker(hash(1), &balance).is_none());
    }
}
