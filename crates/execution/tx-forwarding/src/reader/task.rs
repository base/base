use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, TxHash, U256};
use base_execution_txpool::{
    BasePooledTx, BestTransactionLane, NoExtensions, UnifiedTipOrdering, UnifiedTipPriority,
    ValidatedTransaction, ValidatedTransactionExtensions,
};
use reth_transaction_pool::{
    BlockInfo, PoolTransaction, Priority, TransactionListenerKind, TransactionOrdering,
    TransactionPool, ValidPoolTransaction,
};
use tokio::{sync::mpsc, time};
use tokio_util::sync::CancellationToken;
use tracing::{info, trace};
use url::Url;

use super::{
    LaneMix, LaneScheduler, config::ReaderConfig, metrics::Metrics, validator::RecentlySent,
};
use crate::forwarder::InsertValidatedTransaction;

/// Pool transactions from one snapshot, ranked by the builder's tip ordering and sequenced by
/// nonce lane.
type Snapshot<T> =
    LaneScheduler<Arc<ValidPoolTransaction<T>>, Priority<UnifiedTipPriority>, BestTransactionLane>;

/// The pool reads the forwarding reader makes.
///
/// Narrower than [`TransactionPool`] so tests can control what a snapshot yields and how long it
/// takes; every [`TransactionPool`] implements it.
#[cfg_attr(test, mockall::automock(type Transaction = base_execution_txpool::BasePooledTransaction;))]
pub trait ReaderPool: Send + Sync {
    /// Transaction type the pool holds.
    type Transaction: PoolTransaction;

    /// See [`TransactionPool::block_info`].
    fn block_info(&self) -> BlockInfo;

    /// See [`TransactionPool::best_transactions`].
    fn best_transactions(
        &self,
    ) -> Box<dyn Iterator<Item = Arc<ValidPoolTransaction<Self::Transaction>>> + Send>;

    /// See [`TransactionPool::pending_transactions_listener_for`].
    fn pending_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<TxHash>;

    /// See [`TransactionPool::get`].
    fn get(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>>;

    /// See [`TransactionPool::get_transaction_by_sender_and_nonce`].
    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>>;
}

impl<P: TransactionPool> ReaderPool for P {
    type Transaction = P::Transaction;

    fn block_info(&self) -> BlockInfo {
        TransactionPool::block_info(self)
    }

    fn best_transactions(
        &self,
    ) -> Box<dyn Iterator<Item = Arc<ValidPoolTransaction<Self::Transaction>>> + Send> {
        TransactionPool::best_transactions(self)
    }

    fn pending_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<TxHash> {
        TransactionPool::pending_transactions_listener_for(self, kind)
    }

    fn get(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        TransactionPool::get(self, hash)
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        TransactionPool::get_transaction_by_sender_and_nonce(self, sender, nonce)
    }
}

/// Background reader that drains the pool for one destination.
///
/// Each pass waits until at least half of the destination queue is free, takes a
/// `best_transactions()` snapshot (reusing the previous one for up to
/// [`Self::SNAPSHOT_MAX_AGE`]), and fills the free slots from two lanes: a configured percentage
/// of picks goes to the oldest pending transaction and the rest to the highest tip-per-gas bid (see
/// [`LaneMix`] and [`LaneScheduler`]). Transactions recently accepted by this destination's queue
/// are skipped.
///
/// The order is decided here, when a transaction is queued, and is fixed from then on. Keeping the
/// queue shallow is what lets a high bid that arrives during a burst overtake transactions that
/// are still in the pool.
///
/// A pass that finds nothing to send waits for the pool to announce a new pending transaction,
/// or for [`ReaderConfig::poll_interval`] so that transactions due for a resend are picked up.
/// Waking on the announcement rather than on the interval alone is what keeps an isolated
/// transaction from waiting out the rest of an idle sleep before it is forwarded.
///
/// After each snapshot the reader waits at least as long as that snapshot took before reading
/// the pool again. A snapshot costs a pass over every pending transaction, so without this gap a
/// continuous stream of announcements over a deep pool would keep the reader thread snapshotting
/// nonstop; with it, snapshotting uses at most about half of the thread. While the previous
/// snapshot still holds transactions the reader keeps filling the queue from it during the gap,
/// so the gap only idles the thread once that snapshot is used up. With a small pool the gap is
/// microseconds, so an isolated transaction is not delayed.
///
/// Conversion to the builder-RPC wire form happens here rather than in the
/// forwarder: this is the component that holds a [`ValidPoolTransaction`], and
/// it runs on a dedicated blocking thread, which keeps the EIP-2718 encoding
/// and pool snapshots off the shared async workers.
pub(crate) struct DestinationReader<P: ReaderPool, E = NoExtensions> {
    pool: P,
    config: ReaderConfig,
    recently_sent: RecentlySent,
    /// Highest nonce of each sequence added to the current lanes since the last full read.
    queued: HashMap<BestTransactionLane, u64>,
    /// See [`Self::FULL_SNAPSHOT_INTERVAL`]; a field so tests can pin it.
    full_read_interval: Duration,
    lane_mix: LaneMix,
    sender: mpsc::Sender<InsertValidatedTransaction<E>>,
    cancel: CancellationToken,
    builder_url: Url,
    url_label: Arc<str>,
}

impl<P, E> DestinationReader<P, E>
where
    P: ReaderPool + 'static,
    P::Transaction: BasePooledTx,
    E: ValidatedTransactionExtensions<P::Transaction>,
{
    /// Wait between checks for free queue capacity.
    const CAPACITY_POLL_INTERVAL: Duration = Duration::from_millis(1);

    /// Longest a snapshot is reused before the pool is read again.
    ///
    /// Reading the pool costs a pass over every pending transaction, so under a deep backlog the
    /// reader refills the queue from the previous snapshot instead of re-reading for every free
    /// chunk. A transaction that arrives in the meantime waits up to this long, plus the pacing gap
    /// before the next snapshot, to be considered. That gap is as long as the previous snapshot
    /// took, so it has no fixed ceiling: it grows with the pool.
    const SNAPSHOT_MAX_AGE: Duration = Duration::from_millis(10);

    /// Longest the reader goes between full pool reads while it serves announcements one by one.
    ///
    /// Within this window a wake caused by announcements looks the announced transactions up
    /// individually and adds them to the current snapshot instead of re-reading every pending
    /// transaction to find the few new ones. The next full read picks up whatever announcements
    /// missed (the listener drops them when its channel is full) and transactions due for a
    /// resend. It also adds announced transactions that could not be placed in nonce order (see
    /// [`Self::in_nonce_order`]). This is the normal repair cadence rather than a delivery
    /// ceiling: a full read waits for queue capacity like any other pass.
    const FULL_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(100);

    /// Creates a reader for one destination.
    pub(crate) fn new(
        pool: P,
        config: ReaderConfig,
        sender: mpsc::Sender<InsertValidatedTransaction<E>>,
        cancel: CancellationToken,
        builder_url: Url,
    ) -> Self {
        let recently_sent = RecentlySent::new(config.resend_after);
        let lane_mix = LaneMix::new(config.fifo_percent);
        let url_label = builder_url.to_string().into();
        Self {
            pool,
            config,
            recently_sent,
            queued: HashMap::new(),
            full_read_interval: Self::FULL_SNAPSHOT_INTERVAL,
            lane_mix,
            sender,
            cancel,
            builder_url,
            url_label,
        }
    }

    /// Reader loop — runs until the [`CancellationToken`] is cancelled.
    ///
    /// Meant to be driven on its own blocking thread: snapshots and encoding run synchronously
    /// between awaits.
    pub(crate) async fn run(&mut self) {
        info!(
            builder_url = %self.builder_url,
            resend_after_ms = self.config.resend_after.as_millis() as u64,
            channel_capacity = self.config.channel_capacity,
            poll_interval_ms = self.config.poll_interval.as_millis() as u64,
            fifo_percent = self.config.fifo_percent,
            "starting transaction reader",
        );

        // Subscribed before the first snapshot, so nothing that becomes pending after it is missed.
        let mut pending =
            Some(self.pool.pending_transactions_listener_for(TransactionListenerKind::All));
        // When the last snapshot finished, how long it took, and what is left of it.
        let mut snapshot: Option<(Instant, Duration, Snapshot<P::Transaction>)> = None;
        // When the last full pool read finished and how long it took.
        let mut last_full: Option<(Instant, Duration)> = None;
        // Announced since the last pool read and not yet looked up.
        let mut announced = Vec::new();
        while self.wait_for_capacity().await {
            // A snapshot that still has transactions is reused until the pacing gap after it has
            // passed, so a deep backlog keeps refilling the queue instead of idling in the gap.
            let stale = snapshot.as_ref().is_none_or(|(taken_at, cost, lanes)| {
                lanes.is_empty() || taken_at.elapsed() >= Self::SNAPSHOT_MAX_AGE.max(*cost)
            });
            if stale {
                Self::drain_announcements(&mut pending, &mut announced);
                let recent_full = last_full
                    .is_some_and(|(taken_at, _)| taken_at.elapsed() < self.full_read_interval);
                if recent_full && pending.is_some() {
                    // With nothing announced the current lanes are still the latest view; the
                    // next full read is due when the interval runs out.
                    if !announced.is_empty() {
                        let mut lanes =
                            snapshot.take().map_or_else(LaneScheduler::new, |(_, _, lanes)| lanes);
                        self.push_announced(&mut lanes, &mut announced);
                        snapshot = Some((Instant::now(), Duration::ZERO, lanes));
                    }
                } else {
                    let gap =
                        last_full.and_then(|(taken_at, cost)| cost.checked_sub(taken_at.elapsed()));
                    if let Some(gap) = gap {
                        tokio::select! {
                            () = time::sleep(gap) => {}
                            () = self.cancel.cancelled() => break,
                        }
                    }
                    // Everything announced so far is in the pool, so the snapshot below covers it.
                    Self::drain_announcements(&mut pending, &mut announced);
                    announced.clear();
                    let started = Instant::now();
                    let Some(lanes) = self.take_snapshot() else { break };
                    let taken_at = Instant::now();
                    last_full = Some((taken_at, taken_at - started));
                    snapshot = Some((taken_at, taken_at - started, lanes));
                }
            }
            let Some((_, _, lanes)) = snapshot.as_mut() else { break };

            Metrics::iterations(Arc::clone(&self.url_label)).increment(1);
            match self.fill_queue(lanes) {
                None => return,
                Some(0) => announced.extend(self.wait_for_pending(&mut pending).await),
                Some(_) => {}
            }
        }

        info!("reader cancelled, shutting down");
    }

    /// Reads the pool into a fresh [`Snapshot`], skipping transactions this destination's queue
    /// accepted recently. Returns `None` if the reader was cancelled while reading.
    fn take_snapshot(&mut self) -> Option<Snapshot<P::Transaction>> {
        let mut txs_read: u64 = 0;
        let mut txs_ignored: u64 = 0;

        let base_fee = self.pool.block_info().pending_basefee;
        let ordering = UnifiedTipOrdering::<P::Transaction>::default();
        let mut lanes = LaneScheduler::new();
        self.queued.clear();
        for tx in self.pool.best_transactions() {
            if self.cancel.is_cancelled() {
                info!("reader cancelled during iteration");
                return None;
            }

            txs_read += 1;
            if self.recently_sent.was_recently_sent(tx.hash()) {
                txs_ignored += 1;
                continue;
            }

            let sequence = BestTransactionLane::for_transaction(&tx);
            // A resend ranks below every fresh transaction and ages from when it became due, so
            // the resends a full read finds all at once cannot hold fresh ones back.
            let (arrived, priority) = self.recently_sent.resend_due(tx.hash()).map_or_else(
                || (tx.timestamp, ordering.priority(&tx.transaction, base_fee)),
                |due| (due, Priority::None),
            );
            if let Some(sequence) = sequence {
                self.queued.insert(sequence, tx.nonce());
            }
            lanes.push(tx, sequence, arrived, priority);
        }

        if txs_read > 0 {
            Metrics::txs_read(Arc::clone(&self.url_label)).increment(txs_read);
            Metrics::txs_ignored(Arc::clone(&self.url_label)).increment(txs_ignored);
            Metrics::dedup_cache_size(Arc::clone(&self.url_label))
                .set(self.recently_sent.len() as f64);
            trace!(
                builder_url = %self.builder_url,
                txs_read = txs_read,
                txs_ignored = txs_ignored,
                dedup_cache = self.recently_sent.len(),
                "reader snapshot taken",
            );
        }
        Some(lanes)
    }

    /// Looks up `announced` transactions and adds those still pooled and not recently sent to
    /// `lanes`, in nonce order per sender so each sequence stays gapless. A transaction that
    /// cannot be placed in nonce order is left for the next full read.
    fn push_announced(
        &mut self,
        lanes: &mut Snapshot<P::Transaction>,
        announced: &mut Vec<TxHash>,
    ) {
        let base_fee = self.pool.block_info().pending_basefee;
        let ordering = UnifiedTipOrdering::<P::Transaction>::default();
        let mut txs: Vec<_> = announced.drain(..).filter_map(|hash| self.pool.get(&hash)).collect();
        txs.sort_unstable_by_key(|tx| (tx.sender(), tx.nonce(), *tx.hash()));
        txs.dedup_by_key(|tx| *tx.hash());
        for tx in txs {
            if self.recently_sent.was_recently_sent(tx.hash()) {
                continue;
            }
            let sequence = BestTransactionLane::for_transaction(&tx);
            if let Some(sequence) = sequence {
                if !self.in_nonce_order(&tx, sequence) {
                    continue;
                }
                self.queued.insert(sequence, tx.nonce());
            }
            let arrived = tx.timestamp;
            let priority = ordering.priority(&tx.transaction, base_fee);
            lanes.push(tx, sequence, arrived, priority);
        }
    }

    /// Returns whether an announced `tx` can join the current lanes without the builder seeing its
    /// sequence out of nonce order.
    ///
    /// It can if it replaces or directly follows the sequence's latest transaction in the lanes,
    /// or if its predecessor has been sent or is no longer pooled. It cannot if a later nonce of
    /// the sequence is already in the lanes, since it would be sent after it, or if its
    /// predecessor is pooled but unsent, as when that announcement was dropped. Predecessors in
    /// EIP-8130 keyed nonce lanes cannot be looked up, so those are left to the full read too.
    fn in_nonce_order(
        &self,
        tx: &Arc<ValidPoolTransaction<P::Transaction>>,
        sequence: BestTransactionLane,
    ) -> bool {
        let nonce = tx.nonce();
        if let Some(&last) = self.queued.get(&sequence) {
            if nonce < last {
                return false;
            }
            if nonce - last <= 1 {
                return true;
            }
        }
        let Some(previous) = nonce.checked_sub(1) else { return true };
        if sequence.nonce_key != U256::ZERO {
            return false;
        }
        self.pool
            .get_transaction_by_sender_and_nonce(sequence.sender, previous)
            .is_none_or(|previous| self.recently_sent.was_sent(previous.hash()))
    }

    /// Moves transactions from `lanes` into the destination queue until it is full or `lanes` is
    /// empty, alternating lanes per [`LaneMix`]. Returns how many were queued, or `None` if the
    /// forwarder is gone.
    fn fill_queue(&mut self, lanes: &mut Snapshot<P::Transaction>) -> Option<u64> {
        let mut txs_sent: u64 = 0;
        while self.sender.capacity() > 0 {
            let Some(tx) = lanes.pop(self.lane_mix.next_lane()) else { break };
            let hash = *tx.hash();
            // An announcement can repeat a transaction a full read already put in the lanes.
            if self.recently_sent.was_recently_sent(&hash) {
                continue;
            }
            match self.try_enqueue(&tx) {
                Ok(()) => {
                    self.recently_sent.mark_sent(hash);
                    txs_sent += 1;
                }
                // Only this reader produces into the queue, so capacity cannot shrink between the
                // check above and the send.
                Err(mpsc::error::TrySendError::Full(())) => break,
                Err(mpsc::error::TrySendError::Closed(())) => return None,
            }
        }
        Metrics::txs_sent(Arc::clone(&self.url_label)).increment(txs_sent);
        Some(txs_sent)
    }

    /// Moves queued pending-transaction announcements into `announced` without waiting.
    ///
    /// Drops the listener once the pool closes it, so later waits fall back to the poll interval
    /// instead of spinning on a closed channel.
    fn drain_announcements(
        pending: &mut Option<mpsc::Receiver<TxHash>>,
        announced: &mut Vec<TxHash>,
    ) {
        let Some(receiver) = pending else { return };
        loop {
            match receiver.try_recv() {
                Ok(hash) => announced.push(hash),
                Err(mpsc::error::TryRecvError::Empty) => return,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    *pending = None;
                    return;
                }
            }
        }
    }

    /// Waits until the pool announces a pending transaction, the poll interval elapses, or the
    /// reader is cancelled, whichever comes first. Returns the announced hash, if any.
    ///
    /// An announcement may concern a transaction the last snapshot already covered; such a
    /// spurious wake costs one lookup.
    async fn wait_for_pending(
        &self,
        pending: &mut Option<mpsc::Receiver<TxHash>>,
    ) -> Option<TxHash> {
        let announced = async {
            match pending {
                Some(receiver) => {
                    let hash = receiver.recv().await;
                    if hash.is_none() {
                        *pending = None;
                        time::sleep(self.config.poll_interval).await;
                    }
                    hash
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            hash = announced => hash,
            () = time::sleep(self.config.poll_interval) => None,
            () = self.cancel.cancelled() => None,
        }
    }

    /// Waits until at least half of the destination queue is free.
    ///
    /// Waiting for half rather than one slot means each snapshot fills a meaningful chunk of the
    /// queue instead of rebuilding the lanes for every request the forwarder takes. Returns
    /// `false` if the reader was cancelled or the forwarder is gone.
    ///
    /// Cancellation is checked before capacity, on every call: this is the reader loop's exit,
    /// and an idle reader with a free queue must still take it.
    async fn wait_for_capacity(&self) -> bool {
        let wanted = self.sender.max_capacity().div_ceil(2);
        loop {
            if self.cancel.is_cancelled() || self.sender.is_closed() {
                return false;
            }
            if self.sender.capacity() >= wanted {
                return true;
            }
            time::sleep(Self::CAPACITY_POLL_INTERVAL).await;
        }
    }

    /// Attempts to queue the transaction without waiting on a stale pool snapshot.
    fn try_enqueue(
        &self,
        transaction: &Arc<ValidPoolTransaction<P::Transaction>>,
    ) -> Result<(), mpsc::error::TrySendError<()>> {
        let permit = self.sender.try_reserve()?;
        permit.send(Self::to_wire(transaction));
        Ok(())
    }

    /// Converts a pooled transaction into the request the forwarder relays.
    ///
    /// The bytes are the pooled transaction's cached EIP-2718 encoding, which mempool validation
    /// has already filled, so forwarding shares them instead of cloning and re-encoding.
    fn to_wire(
        transaction: &Arc<ValidPoolTransaction<P::Transaction>>,
    ) -> InsertValidatedTransaction<E> {
        InsertValidatedTransaction {
            transaction: ValidatedTransaction {
                sender: *transaction.sender_ref(),
                raw: BasePooledTx::encoded_2718(&transaction.transaction).into_owned(),
                metering: transaction.transaction.metering().cloned(),
                extensions: E::extract(transaction),
            },
            tx_hash: *transaction.transaction.hash(),
        }
    }
}

impl<P: ReaderPool, E> fmt::Debug for DestinationReader<P, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DestinationReader")
            .field("builder_url", &self.builder_url)
            .field("config", &self.config)
            .field("recently_sent", &self.recently_sent)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559, transaction::Recovered};
    use alloy_primitives::{Address, B256, Signature, TxKind, U256};
    use base_common_consensus::{BaseTransactionSigned, TxDeposit};
    use base_execution_txpool::BasePooledTransaction;
    use reth_transaction_pool::{
        TransactionOrigin, identifier::TransactionId, noop::NoopTransactionPool,
    };

    use super::*;
    use crate::reader::lanes::Lane;

    /// Start and end of one `best_transactions()` call.
    type SnapshotSpan = (Instant, Instant);

    fn transaction(nonce: u64) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let sender = Address::repeat_byte((nonce + 1) as u8);
        let signed: BaseTransactionSigned = TxDeposit {
            source_hash: B256::with_last_byte(nonce as u8),
            from: sender,
            to: TxKind::Call(Address::repeat_byte(0x42)),
            mint: 0,
            value: U256::from(nonce),
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Default::default(),
        }
        .into();
        let transaction = BasePooledTransaction::new(Recovered::new_unchecked(signed, sender), 128);
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), nonce),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    /// An EIP-1559 transaction from sender `sender` at `nonce`; another `salt` with the same
    /// sender and nonce makes a replacement.
    fn from_sender(
        sender: u8,
        nonce: u64,
        salt: u8,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let sender = Address::repeat_byte(sender);
        let signed: BaseTransactionSigned = TxEip1559 {
            chain_id: 1,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(Address::repeat_byte(0x42)),
            value: U256::from(salt),
            access_list: Default::default(),
            input: Default::default(),
        }
        .into_signed(Signature::test_signature())
        .into();
        let transaction = BasePooledTransaction::new(Recovered::new_unchecked(signed, sender), 128);
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), nonce),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    type TestReader = DestinationReader<NoopTransactionPool<BasePooledTransaction>>;

    /// The queued form of `transaction(nonce)`, for seeding a queue directly.
    fn wire(nonce: u64) -> InsertValidatedTransaction {
        TestReader::to_wire(&transaction(nonce))
    }

    fn reader(
        sender: mpsc::Sender<InsertValidatedTransaction>,
        cancel: CancellationToken,
    ) -> TestReader {
        reader_for("http://builder.test", sender, cancel)
    }

    fn reader_for(
        url: &str,
        sender: mpsc::Sender<InsertValidatedTransaction>,
        cancel: CancellationToken,
    ) -> TestReader {
        DestinationReader::new(
            NoopTransactionPool::new(),
            ReaderConfig {
                resend_after: Duration::from_secs(4),
                channel_capacity: 1,
                poll_interval: Duration::from_millis(1),
                fifo_percent: 20,
            },
            sender,
            cancel,
            url.parse().unwrap(),
        )
    }

    /// The wire form must carry the fields the builder RPC needs, or the queue would move
    /// well-formed-looking rows that the destination rejects.
    #[test]
    fn to_wire_carries_the_sender_hash_and_encoded_bytes() {
        let transaction = transaction(3);

        let converted = TestReader::to_wire(&transaction);

        assert_eq!(converted.tx_hash, *transaction.hash());
        assert_eq!(converted.transaction.sender, *transaction.sender_ref());
        assert!(converted.transaction.metering.is_none());
        assert!(!converted.transaction.raw.is_empty(), "the envelope must be encoded");
    }

    #[test]
    fn full_queue_leaves_the_transaction_for_a_fresh_snapshot() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender.try_send(wire(0)).unwrap();
        let expected = transaction(1);
        let reader = reader(sender, CancellationToken::new());

        assert!(matches!(reader.try_enqueue(&expected), Err(mpsc::error::TrySendError::Full(()))));
        assert_eq!(receiver.try_recv().unwrap().tx_hash, wire(0).tx_hash);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn closed_queue_stops_the_reader() {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let reader = reader(sender, CancellationToken::new());

        assert!(matches!(
            reader.try_enqueue(&transaction(1)),
            Err(mpsc::error::TrySendError::Closed(()))
        ));
    }

    /// A pool whose snapshots block for `cost` and yield `txs`, announcing whatever arrives on
    /// `announcements` and reporting each snapshot's span on `snapshots`.
    fn costly_pool(
        cost: Duration,
        txs: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
        announcements: mpsc::Receiver<TxHash>,
        snapshots: mpsc::UnboundedSender<SnapshotSpan>,
    ) -> MockReaderPool {
        let mut pool = MockReaderPool::new();
        pool.expect_block_info().returning(BlockInfo::default);
        pool.expect_pending_transactions_listener_for().return_once(move |_| announcements);
        pool.expect_get().returning(|_| None);
        pool.expect_best_transactions().returning(move || {
            let started = Instant::now();
            std::thread::sleep(cost);
            let _ = snapshots.send((started, Instant::now()));
            Box::new(txs.clone().into_iter())
        });
        pool
    }

    /// A reader over `pool` that wakes only on announcements, never on the poll interval.
    fn paced_reader(
        pool: MockReaderPool,
        sender: mpsc::Sender<InsertValidatedTransaction>,
        cancel: CancellationToken,
    ) -> DestinationReader<MockReaderPool> {
        DestinationReader::new(
            pool,
            ReaderConfig {
                resend_after: Duration::from_secs(60),
                channel_capacity: 64,
                poll_interval: Duration::from_secs(60),
                fifo_percent: 20,
            },
            sender,
            cancel,
            "http://builder.test".parse().unwrap(),
        )
    }

    /// Every wake under a steady stream of announcements finds nothing new to send, so without
    /// pacing the reader would snapshot back to back; each snapshot must instead be followed by a
    /// gap at least as long as it took.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continuous_announcements_pace_snapshots_by_their_cost() {
        const COST: Duration = Duration::from_millis(5);
        let txs: Vec<_> = (0..4).map(transaction).collect();
        let (announce, announcements) = mpsc::channel(16);
        let (snapshot_tx, mut snapshots) = mpsc::unbounded_channel();
        let (sender, mut queue) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let mut reader = paced_reader(
            costly_pool(COST, txs.clone(), announcements, snapshot_tx),
            sender,
            cancel.clone(),
        );
        // A plain thread, so announcements keep coming while a snapshot blocks a runtime worker.
        let announcer = std::thread::spawn(move || {
            while !announce.is_closed() {
                let _ = announce.try_send(TxHash::ZERO);
                std::thread::sleep(Duration::from_micros(100));
            }
        });

        let run = tokio::spawn(async move { reader.run().await });
        // Waits for spans rather than a fixed time, so a slow machine only makes the test longer.
        let mut spans = Vec::new();
        time::timeout(Duration::from_secs(5), async {
            while spans.len() < 3 {
                spans.push(snapshots.recv().await.expect("the reader keeps snapshotting"));
            }
        })
        .await
        .expect("three snapshots are taken");
        cancel.cancel();
        time::timeout(Duration::from_secs(5), run).await.expect("reader exits").unwrap();
        announcer.join().unwrap();

        while let Ok(span) = snapshots.try_recv() {
            spans.push(span);
        }
        assert!(spans.len() >= 3, "too few snapshots to judge pacing: {}", spans.len());
        for pair in spans.windows(2) {
            let ((started, finished), (next, _)) = (pair[0], pair[1]);
            assert!(
                next - finished >= finished - started,
                "snapshot taking {:?} was followed after only {:?}",
                finished - started,
                next - finished,
            );
        }
        let mut delivered = Vec::new();
        while let Ok(forwarded) = queue.try_recv() {
            delivered.push(forwarded.tx_hash);
        }
        delivered.sort();
        let mut expected: Vec<_> = txs.iter().map(|tx| *tx.hash()).collect();
        expected.sort();
        assert_eq!(delivered, expected, "every transaction is forwarded exactly once");
    }

    /// A snapshot that still holds transactions keeps refilling the queue through the pacing gap,
    /// so a backlog larger than the queue drains from one snapshot instead of idling until the
    /// next one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backlog_keeps_refilling_from_the_snapshot_during_the_gap() {
        // Draining the backlog takes well past `SNAPSHOT_MAX_AGE` but well short of this cost.
        const COST: Duration = Duration::from_millis(500);
        const BACKLOG: usize = 40;
        let txs: Vec<_> = (0..BACKLOG as u64).map(transaction).collect();
        let (_announce, announcements) = mpsc::channel(16);
        let (snapshot_tx, mut snapshots) = mpsc::unbounded_channel();
        let (sender, mut queue) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let mut reader = paced_reader(
            costly_pool(COST, txs.clone(), announcements, snapshot_tx),
            sender,
            cancel.clone(),
        );

        let run = tokio::spawn(async move { reader.run().await });
        let mut delivered = Vec::new();
        time::timeout(Duration::from_secs(5), async {
            while delivered.len() < BACKLOG {
                delivered.push(queue.recv().await.expect("the reader keeps forwarding").tx_hash);
                time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the whole backlog is forwarded");
        let snapshots_taken = std::iter::from_fn(|| snapshots.try_recv().ok()).count();
        cancel.cancel();
        time::timeout(Duration::from_secs(5), run).await.expect("reader exits").unwrap();

        assert_eq!(snapshots_taken, 1, "the backlog drains from the first snapshot");
        delivered.sort();
        let mut expected: Vec<_> = txs.iter().map(|tx| *tx.hash()).collect();
        expected.sort();
        assert_eq!(delivered, expected, "every transaction is forwarded exactly once");
    }

    /// Shortly after a full read, an announced transaction is looked up and forwarded on its own
    /// instead of by reading every pending transaction again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_announcement_soon_after_a_full_read_is_forwarded_by_lookup() {
        let tx = transaction(0);
        let (announce, announcements) = mpsc::channel(16);
        let (read_tx, mut reads) = mpsc::unbounded_channel();
        let mut pool = MockReaderPool::new();
        pool.expect_block_info().returning(BlockInfo::default);
        pool.expect_pending_transactions_listener_for().return_once(move |_| announcements);
        pool.expect_best_transactions().returning(move || {
            let _ = read_tx.send(());
            Box::new(std::iter::empty())
        });
        let pooled = Arc::clone(&tx);
        pool.expect_get()
            .returning(move |hash| (hash == pooled.hash()).then(|| Arc::clone(&pooled)));
        let (sender, mut queue) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let mut reader = paced_reader(pool, sender, cancel.clone());
        // No second full read can come due during the test, so only a lookup can forward it.
        reader.full_read_interval = Duration::from_secs(3600);

        let run = tokio::spawn(async move { reader.run().await });
        reads.recv().await.expect("the reader starts with a full read");
        announce.send(*tx.hash()).await.unwrap();
        let forwarded = time::timeout(Duration::from_secs(5), queue.recv())
            .await
            .expect("the announced transaction is forwarded")
            .unwrap();
        cancel.cancel();
        time::timeout(Duration::from_secs(5), run).await.expect("reader exits").unwrap();

        assert_eq!(forwarded.tx_hash, *tx.hash());
        assert!(reads.try_recv().is_err(), "no second full read was needed");
    }

    /// A successor announced while its predecessor is pooled but unsent, as when the
    /// predecessor's announcement was dropped, must wait for the full read rather than reach the
    /// builder ahead of it; once the predecessor has been sent, the successor goes by lookup.
    #[test]
    fn an_announced_successor_waits_until_its_predecessor_was_sent() {
        let (previous, next) = (from_sender(0xaa, 4, 1), from_sender(0xaa, 5, 2));
        let mut pool = MockReaderPool::new();
        pool.expect_block_info().returning(BlockInfo::default);
        let pooled = Arc::clone(&next);
        pool.expect_get()
            .returning(move |hash| (hash == pooled.hash()).then(|| Arc::clone(&pooled)));
        let pooled = Arc::clone(&previous);
        pool.expect_get_transaction_by_sender_and_nonce()
            .returning(move |_, nonce| (nonce == 4).then(|| Arc::clone(&pooled)));
        let (sender, _queue) = mpsc::channel(4);
        let mut reader = paced_reader(pool, sender, CancellationToken::new());
        let mut lanes = LaneScheduler::new();

        reader.push_announced(&mut lanes, &mut vec![*next.hash()]);
        assert!(lanes.is_empty(), "held back while the predecessor is unsent");

        reader.recently_sent.mark_sent(*previous.hash());
        reader.push_announced(&mut lanes, &mut vec![*next.hash()]);
        assert_eq!(lanes.pop(Lane::Priority).map(|tx| *tx.hash()), Some(*next.hash()));
    }

    /// A replacement announced while later nonces of its sender wait in the lanes would be sent
    /// after them, so it is left for the full read, which orders the sequence again.
    #[test]
    fn a_replacement_behind_later_nonces_waits_for_the_full_read() {
        let (first, second) = (from_sender(0xaa, 0, 1), from_sender(0xaa, 1, 2));
        let replacement = from_sender(0xaa, 0, 3);
        let mut pool = MockReaderPool::new();
        pool.expect_block_info().returning(BlockInfo::default);
        let txs = vec![Arc::clone(&first), Arc::clone(&second)];
        pool.expect_best_transactions().returning(move || Box::new(txs.clone().into_iter()));
        let pooled = Arc::clone(&replacement);
        pool.expect_get()
            .returning(move |hash| (hash == pooled.hash()).then(|| Arc::clone(&pooled)));
        let (sender, _queue) = mpsc::channel(4);
        let mut reader = paced_reader(pool, sender, CancellationToken::new());
        let mut lanes = reader.take_snapshot().unwrap();

        reader.push_announced(&mut lanes, &mut vec![*replacement.hash()]);

        let order: Vec<_> =
            std::iter::from_fn(|| lanes.pop(Lane::Fifo)).map(|tx| *tx.hash()).collect();
        assert_eq!(order, vec![*first.hash(), *second.hash()]);
    }

    /// The pool drops announcements when a listener's channel is full, so a transaction that
    /// becomes pending without one must still be forwarded by a later full read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_transaction_without_an_announcement_is_forwarded_by_a_later_full_read() {
        let tx = transaction(0);
        let (_announce, announcements) = mpsc::channel(16);
        let mut pool = MockReaderPool::new();
        pool.expect_block_info().returning(BlockInfo::default);
        pool.expect_pending_transactions_listener_for().return_once(move |_| announcements);
        pool.expect_get().returning(|_| None);
        // Pending only after the first read, and never announced.
        let pending = Arc::clone(&tx);
        let mut first = true;
        pool.expect_best_transactions().returning(move || {
            let txs =
                if std::mem::take(&mut first) { Vec::new() } else { vec![Arc::clone(&pending)] };
            Box::new(txs.into_iter())
        });
        let (sender, mut queue) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let mut reader: DestinationReader<MockReaderPool> = DestinationReader::new(
            pool,
            ReaderConfig {
                resend_after: Duration::from_secs(60),
                channel_capacity: 64,
                poll_interval: ReaderConfig::POLL_INTERVAL,
                fifo_percent: 20,
            },
            sender,
            cancel.clone(),
            "http://builder.test".parse().unwrap(),
        );

        let run = tokio::spawn(async move { reader.run().await });
        let forwarded = time::timeout(Duration::from_secs(5), queue.recv())
            .await
            .expect("the unannounced transaction is forwarded")
            .unwrap();
        cancel.cancel();
        time::timeout(Duration::from_secs(5), run).await.expect("reader exits").unwrap();

        assert_eq!(forwarded.tx_hash, *tx.hash());
    }

    /// The resends a full read finds rank below fresh transactions, so a burst of them cannot
    /// hold a fresh transaction back, even when the resend entered the pool first.
    #[test]
    fn a_full_read_ranks_a_resend_below_a_fresh_transaction() {
        let (resend, fresh) = (transaction(0), transaction(1));
        let mut pool = MockReaderPool::new();
        pool.expect_block_info().returning(BlockInfo::default);
        let txs = vec![Arc::clone(&resend), Arc::clone(&fresh)];
        pool.expect_best_transactions().returning(move || Box::new(txs.clone().into_iter()));
        let (sender, _queue) = mpsc::channel(4);
        let mut reader: DestinationReader<MockReaderPool> = DestinationReader::new(
            pool,
            ReaderConfig {
                resend_after: Duration::ZERO,
                channel_capacity: 4,
                poll_interval: ReaderConfig::POLL_INTERVAL,
                fifo_percent: 0,
            },
            sender,
            CancellationToken::new(),
            "http://builder.test".parse().unwrap(),
        );
        reader.recently_sent.mark_sent(*resend.hash());

        let mut lanes = reader.take_snapshot().unwrap();

        assert_eq!(lanes.pop(Lane::Priority).map(|tx| *tx.hash()), Some(*fresh.hash()));
    }

    /// Cancelling while the reader waits out the gap after an expensive snapshot must end the
    /// reader at once rather than after the gap or another snapshot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_during_the_pacing_gap_returns_promptly() {
        // The join bound must stay well below the cost, or a reader that starts another snapshot
        // instead of waiting out the gap would still pass.
        const COST: Duration = Duration::from_secs(2);
        let (_announce, announcements) = mpsc::channel(16);
        let (snapshot_tx, _snapshots) = mpsc::unbounded_channel();
        let (sender, mut queue) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let mut reader = paced_reader(
            costly_pool(COST, vec![transaction(0)], announcements, snapshot_tx),
            sender,
            cancel.clone(),
        );

        let run = tokio::spawn(async move { reader.run().await });
        queue.recv().await.expect("the first snapshot's transaction is forwarded");
        // Long enough to be well into whatever follows the first snapshot, short of its cost.
        time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();

        time::timeout(Duration::from_millis(500), run)
            .await
            .expect("reader exits well before the gap or another snapshot ends")
            .unwrap();
    }
}
