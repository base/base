//! Listener publication for lane-store lifecycle transitions.

use std::sync::Arc;

use alloy_primitives::{TxHash, map::HashMap};
use parking_lot::Mutex;
use reth_transaction_pool::{
    AllTransactionsEvents, FullTransactionEvent, NewTransactionEvent, PropagatedTransactions,
    SubPool, TransactionEvent, TransactionEvents, TransactionListenerKind, pool::QueuedReason,
};
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::{
    BasePooledTx, LaneGap, LaneTerminalEvent, LaneTransactionState, LaneTransactionTransition,
    LaneTransitionBatch,
};

/// Default capacity of bounded lane event listener channels.
pub const DEFAULT_LANE_EVENT_CHANNEL_CAPACITY: usize = 1_024;

/// Reusable Reth-compatible listener hub for lane-store lifecycle batches.
#[derive(Debug)]
pub struct LaneEventHub<T: BasePooledTx> {
    inner: Mutex<LaneEventHubInner<T>>,
    channel_capacity: usize,
}

impl<T: BasePooledTx> Default for LaneEventHub<T> {
    fn default() -> Self {
        Self::new(DEFAULT_LANE_EVENT_CHANNEL_CAPACITY)
    }
}

impl<T: BasePooledTx> LaneEventHub<T> {
    /// Creates a listener hub with the given bounded-listener capacity.
    pub fn new(channel_capacity: usize) -> Self {
        assert!(channel_capacity > 0, "lane event channels require non-zero capacity");
        Self { inner: Mutex::new(LaneEventHubInner::default()), channel_capacity }
    }

    /// Subscribes to lifecycle events for one transaction hash.
    pub fn transaction_event_listener(&self, hash: TxHash) -> TransactionEvents {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.inner.lock().by_hash.entry(hash).or_default().push(sender);
        TransactionEvents::new(hash, receiver)
    }

    /// Subscribes to full lifecycle events for every transaction.
    pub fn all_transactions_event_listener(&self) -> AllTransactionsEvents<T> {
        let (sender, receiver) = mpsc::channel(self.channel_capacity);
        self.inner.lock().all.push(sender);
        AllTransactionsEvents::new(receiver)
    }

    /// Subscribes to hashes whenever transactions enter the pending subpool.
    pub fn pending_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<TxHash> {
        let (sender, receiver) = mpsc::channel(self.channel_capacity);
        let mut inner = self.inner.lock();
        if kind.is_propagate_only() {
            inner.pending_propagate.push(sender);
        } else {
            inner.pending_all.push(sender);
        }
        receiver
    }

    /// Subscribes whenever transactions enter a Reth subpool.
    pub fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<T>> {
        let (sender, receiver) = mpsc::channel(self.channel_capacity);
        let mut inner = self.inner.lock();
        if kind.is_propagate_only() {
            inner.new_propagate.push(sender);
        } else {
            inner.new_all.push(sender);
        }
        receiver
    }

    /// Publishes a completed transition batch.
    ///
    /// Callers should invoke this only after releasing any mutable store guard.
    pub fn publish(&self, batch: &LaneTransitionBatch<T>) {
        let mut inner = self.inner.lock();
        for transition in &batch.transitions {
            inner.publish_transition(transition);
        }
    }

    /// Publishes a terminal invalid event for a transaction rejected during validation.
    pub fn publish_invalid(&self, hash: TxHash) {
        let mut inner = self.inner.lock();
        inner.broadcast_hash(hash, TransactionEvent::Invalid);
        inner.broadcast_all(FullTransactionEvent::Invalid(hash));
    }

    /// Publishes a terminal discarded event for a transaction that could not be validated.
    pub fn publish_discarded(&self, hash: TxHash) {
        let mut inner = self.inner.lock();
        inner.broadcast_hash(hash, TransactionEvent::Discarded);
        inner.broadcast_all(FullTransactionEvent::Discarded(hash));
    }

    /// Publishes propagation metadata for known transaction hashes.
    pub fn publish_propagated(&self, propagated: PropagatedTransactions) {
        let mut inner = self.inner.lock();
        for (hash, peers) in propagated {
            let peers = Arc::new(peers);
            inner.broadcast_hash(hash, TransactionEvent::Propagated(Arc::clone(&peers)));
            inner.broadcast_all(FullTransactionEvent::Propagated(peers));
        }
    }

    /// Publishes mining for a transaction that was speculatively pruned earlier.
    pub fn publish_mined(&self, hash: TxHash, block_hash: alloy_primitives::B256) {
        let mut inner = self.inner.lock();
        inner.broadcast_hash(hash, TransactionEvent::Mined(block_hash));
        inner.broadcast_all(FullTransactionEvent::Mined { tx_hash: hash, block_hash });
    }
}

#[derive(Debug)]
struct LaneEventHubInner<T: BasePooledTx> {
    by_hash: HashMap<TxHash, Vec<mpsc::UnboundedSender<TransactionEvent>>>,
    all: Vec<mpsc::Sender<FullTransactionEvent<T>>>,
    pending_all: Vec<mpsc::Sender<TxHash>>,
    pending_propagate: Vec<mpsc::Sender<TxHash>>,
    new_all: Vec<mpsc::Sender<NewTransactionEvent<T>>>,
    new_propagate: Vec<mpsc::Sender<NewTransactionEvent<T>>>,
}

impl<T: BasePooledTx> Default for LaneEventHubInner<T> {
    fn default() -> Self {
        Self {
            by_hash: HashMap::default(),
            all: Vec::new(),
            pending_all: Vec::new(),
            pending_propagate: Vec::new(),
            new_all: Vec::new(),
            new_propagate: Vec::new(),
        }
    }
}

impl<T: BasePooledTx> LaneEventHubInner<T> {
    fn publish_transition(&mut self, transition: &LaneTransactionTransition<T>) {
        let hash = *transition.transaction.hash();
        if let Some(terminal) = transition.terminal {
            self.publish_terminal(transition, terminal);
            return;
        }

        if transition.previous_state == transition.current_state {
            return;
        }
        let Some(state) = transition.current_state else { return };
        let (subpool, event, full) = match state {
            LaneTransactionState::Pending => {
                (SubPool::Pending, TransactionEvent::Pending, FullTransactionEvent::Pending(hash))
            }
            LaneTransactionState::BaseFee => (
                SubPool::BaseFee,
                TransactionEvent::Queued,
                FullTransactionEvent::Queued(hash, Some(QueuedReason::InsufficientBaseFee)),
            ),
            LaneTransactionState::Funding(_) => (
                SubPool::Queued,
                TransactionEvent::Queued,
                FullTransactionEvent::Queued(hash, Some(QueuedReason::InsufficientBalance)),
            ),
            LaneTransactionState::Queued(gap) => (
                SubPool::Queued,
                TransactionEvent::Queued,
                FullTransactionEvent::Queued(hash, Some(Self::queued_reason(gap))),
            ),
        };
        self.broadcast_hash(hash, event);
        self.broadcast_all(full);

        if subpool == SubPool::Pending {
            Self::send_bounded(&mut self.pending_all, hash);
            if transition.transaction.propagate {
                Self::send_bounded(&mut self.pending_propagate, hash);
            }
        }

        let initial = transition.previous_state.is_none();
        let pending_promotion = subpool == SubPool::Pending
            && transition
                .previous_state
                .is_some_and(|state| state != LaneTransactionState::Pending);
        if initial || pending_promotion {
            let new_event =
                NewTransactionEvent { subpool, transaction: Arc::clone(&transition.transaction) };
            Self::send_bounded(&mut self.new_all, new_event.clone());
            if transition.transaction.propagate {
                Self::send_bounded(&mut self.new_propagate, new_event);
            }
        }
    }

    const fn queued_reason(gap: LaneGap) -> QueuedReason {
        match gap {
            LaneGap::Missing { .. } => QueuedReason::NonceGap,
            LaneGap::BlockedByBaseFee { .. } | LaneGap::BlockedByFunding { .. } => {
                QueuedReason::ParkedAncestors
            }
        }
    }

    fn publish_terminal(
        &mut self,
        transition: &LaneTransactionTransition<T>,
        terminal: LaneTerminalEvent,
    ) {
        let hash = *transition.transaction.hash();
        let (event, full) = match terminal {
            LaneTerminalEvent::Replaced { by } => (
                TransactionEvent::Replaced(by),
                FullTransactionEvent::Replaced {
                    transaction: Arc::clone(&transition.transaction),
                    replaced_by: by,
                },
            ),
            LaneTerminalEvent::Mined { block_hash } => (
                TransactionEvent::Mined(block_hash),
                FullTransactionEvent::Mined { tx_hash: hash, block_hash },
            ),
            LaneTerminalEvent::Invalid => {
                (TransactionEvent::Invalid, FullTransactionEvent::Invalid(hash))
            }
            LaneTerminalEvent::Removed
            | LaneTerminalEvent::Expired
            | LaneTerminalEvent::Evicted
            | LaneTerminalEvent::Committed => {
                (TransactionEvent::Discarded, FullTransactionEvent::Discarded(hash))
            }
        };
        self.broadcast_hash(hash, event);
        self.broadcast_all(full);
    }

    fn broadcast_hash(&mut self, hash: TxHash, event: TransactionEvent) {
        let Some(listeners) = self.by_hash.get_mut(&hash) else { return };
        listeners.retain(|listener| listener.send(event.clone()).is_ok() && !event.is_final());
        if listeners.is_empty() {
            self.by_hash.remove(&hash);
        }
    }

    fn broadcast_all(&mut self, event: FullTransactionEvent<T>) {
        Self::send_bounded(&mut self.all, event);
    }

    fn send_bounded<E: Clone>(listeners: &mut Vec<mpsc::Sender<E>>, event: E) {
        listeners.retain(|listener| match listener.try_send(event.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Closed(_)) => false,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

    use alloy_consensus::transaction::Recovered;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::B256;
    use base_common_chains::ChainConfig;
    use base_common_consensus::BaseTransactionSigned;
    use base_test_utils::Account;
    use futures::StreamExt;
    use reth_transaction_pool::{
        FullTransactionEvent, SubPool, TransactionEvent, TransactionListenerKind,
        TransactionOrigin,
        identifier::{SenderId, TransactionId},
        pool::QueuedReason,
        test_utils::TransactionBuilder,
    };

    use super::*;
    use crate::{BasePooledTransaction, LaneGap, LaneTerminalEvent, LaneTransitionCause};

    fn transaction(
        account: Account,
        nonce: u64,
        max_fee_per_gas: u128,
    ) -> Arc<reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>> {
        let signed = TransactionBuilder::default()
            .signer(account.signer_b256())
            .chain_id(ChainConfig::mainnet().chain_id)
            .nonce(nonce)
            .to(Account::Bob.address())
            .value(1_000)
            .gas_limit(21_000)
            .max_priority_fee_per_gas(max_fee_per_gas / 10)
            .max_fee_per_gas(max_fee_per_gas)
            .into_eip1559();
        let transaction = BaseTransactionSigned::Eip1559(
            signed.as_eip1559().expect("EIP-1559 transaction").clone(),
        );
        let recovered = Recovered::new_unchecked(transaction, account.address());
        let encoded_length = recovered.encode_2718_len();
        let transaction = BasePooledTransaction::new(recovered, encoded_length);
        Arc::new(reth_transaction_pool::ValidPoolTransaction {
            transaction_id: TransactionId::new(SenderId::from(1), nonce),
            transaction,
            propagate: false,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    fn batch(
        transaction: Arc<reth_transaction_pool::ValidPoolTransaction<BasePooledTransaction>>,
        previous_state: Option<LaneTransactionState>,
        current_state: Option<LaneTransactionState>,
        terminal: Option<LaneTerminalEvent>,
    ) -> LaneTransitionBatch<BasePooledTransaction> {
        LaneTransitionBatch {
            cause: LaneTransitionCause::Insert,
            transitions: vec![LaneTransactionTransition {
                transaction,
                previous_state,
                current_state,
                previous_funding: None,
                current_funding: None,
                terminal,
            }],
        }
    }

    #[tokio::test]
    async fn listener_mapping_matches_reth_subpool_and_terminal_events() {
        let hub = LaneEventHub::default();
        let mut all = hub.all_transactions_event_listener();
        let mut pending_all = hub.pending_transactions_listener_for(TransactionListenerKind::All);
        let mut pending_propagate =
            hub.pending_transactions_listener_for(TransactionListenerKind::PropagateOnly);
        let mut new_all = hub.new_transactions_listener_for(TransactionListenerKind::All);
        let pending_tx = transaction(Account::Alice, 0, 100);
        let hash = *pending_tx.hash();

        hub.publish(&batch(
            Arc::clone(&pending_tx),
            None,
            Some(LaneTransactionState::Pending),
            None,
        ));
        assert!(
            matches!(all.next().await, Some(FullTransactionEvent::Pending(event)) if event == hash)
        );
        assert_eq!(pending_all.recv().await, Some(hash));
        assert!(pending_propagate.try_recv().is_err());
        assert_eq!(new_all.recv().await.unwrap().subpool, SubPool::Pending);

        hub.publish(&batch(
            Arc::clone(&pending_tx),
            Some(LaneTransactionState::Pending),
            Some(LaneTransactionState::BaseFee),
            None,
        ));
        assert!(matches!(
            all.next().await,
            Some(FullTransactionEvent::Queued(event, Some(QueuedReason::InsufficientBaseFee)))
                if event == hash
        ));
        assert!(new_all.try_recv().is_err());

        hub.publish(&batch(
            pending_tx,
            Some(LaneTransactionState::BaseFee),
            Some(LaneTransactionState::Pending),
            None,
        ));
        assert!(
            matches!(all.next().await, Some(FullTransactionEvent::Pending(event)) if event == hash)
        );
        assert_eq!(pending_all.recv().await, Some(hash));
        assert_eq!(new_all.recv().await.unwrap().subpool, SubPool::Pending);

        let expired = transaction(Account::Bob, 0, 100);
        let expired_hash = *expired.hash();
        hub.publish(&batch(
            expired,
            Some(LaneTransactionState::Pending),
            None,
            Some(LaneTerminalEvent::Expired),
        ));
        assert!(matches!(
            all.next().await,
            Some(FullTransactionEvent::Discarded(event)) if event == expired_hash
        ));

        let committed = transaction(Account::Charlie, 0, 100);
        let committed_hash = *committed.hash();
        hub.publish(&batch(
            committed,
            Some(LaneTransactionState::Queued(LaneGap::Missing { expected: 0, found: 1 })),
            None,
            Some(LaneTerminalEvent::Committed),
        ));
        assert!(matches!(
            all.next().await,
            Some(FullTransactionEvent::Discarded(event)) if event == committed_hash
        ));
    }

    #[tokio::test]
    async fn bounded_listener_backpressure_drops_only_the_full_channel_event() {
        let hub = LaneEventHub::new(1);
        let mut listener = hub.new_transactions_listener_for(TransactionListenerKind::All);
        let first = transaction(Account::Alice, 0, 100);
        let first_hash = *first.hash();
        let second = transaction(Account::Bob, 0, 100);
        let third = transaction(Account::Charlie, 0, 100);
        let third_hash = *third.hash();

        hub.publish(&batch(first, None, Some(LaneTransactionState::Pending), None));
        hub.publish(&batch(second, None, Some(LaneTransactionState::Pending), None));
        assert_eq!(*listener.recv().await.unwrap().transaction.hash(), first_hash);

        hub.publish(&batch(third, None, Some(LaneTransactionState::Pending), None));
        assert_eq!(*listener.recv().await.unwrap().transaction.hash(), third_hash);
        assert!(listener.try_recv().is_err());
    }

    #[tokio::test]
    async fn by_hash_terminal_event_closes_the_listener() {
        let hub = LaneEventHub::default();
        let original = transaction(Account::Alice, 0, 100);
        let original_hash = *original.hash();
        let replacement_hash = B256::repeat_byte(0xa1);
        let mut listener = hub.transaction_event_listener(original_hash);

        hub.publish(&batch(Arc::clone(&original), None, Some(LaneTransactionState::Pending), None));
        hub.publish(&batch(
            original,
            Some(LaneTransactionState::Pending),
            None,
            Some(LaneTerminalEvent::Replaced { by: replacement_hash }),
        ));

        assert_eq!(listener.next().await, Some(TransactionEvent::Pending));
        assert_eq!(listener.next().await, Some(TransactionEvent::Replaced(replacement_hash)));
        assert_eq!(listener.next().await, None);
    }
}
