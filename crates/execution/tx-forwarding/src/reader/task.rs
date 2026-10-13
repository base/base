use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::TxHash;
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

/// The pool reads a [`DestinationReader`] makes.
///
/// Narrower than [`TransactionPool`] so tests can control what a snapshot yields and how long it
/// takes; every [`TransactionPool`] implements it.
#[cfg_attr(test, mockall::automock(type Transaction = base_execution_txpool::BasePooledTransaction;))]
pub(crate) trait ReaderPool: Send + Sync {
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
/// nonstop; with it, snapshotting uses at most about half of the thread. With a small pool the gap
/// is microseconds, so an isolated transaction is not delayed.
///
/// Conversion to the builder-RPC wire form happens here rather than in the
/// forwarder: this is the component that holds a [`ValidPoolTransaction`], and
/// it runs on a dedicated blocking thread, which keeps the EIP-2718 encoding
/// and pool snapshots off the shared async workers.
pub(crate) struct DestinationReader<P: ReaderPool, E = NoExtensions> {
    pool: P,
    config: ReaderConfig,
    recently_sent: RecentlySent,
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
        Self { pool, config, recently_sent, lane_mix, sender, cancel, builder_url, url_label }
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
        while self.wait_for_capacity().await {
            let stale = snapshot.as_ref().is_none_or(|(taken_at, _, lanes)| {
                lanes.is_empty() || taken_at.elapsed() >= Self::SNAPSHOT_MAX_AGE
            });
            if stale {
                let gap = snapshot
                    .as_ref()
                    .and_then(|(taken_at, cost, _)| cost.checked_sub(taken_at.elapsed()));
                if let Some(gap) = gap {
                    tokio::select! {
                        () = time::sleep(gap) => {}
                        () = self.cancel.cancelled() => break,
                    }
                }
                // Everything announced so far is in the pool, so the snapshot below covers it.
                Self::drain_announcements(&mut pending);
                let started = Instant::now();
                let Some(lanes) = self.take_snapshot() else { break };
                let taken_at = Instant::now();
                snapshot = Some((taken_at, taken_at - started, lanes));
            }
            let Some((_, _, lanes)) = snapshot.as_mut() else { break };

            Metrics::iterations(Arc::clone(&self.url_label)).increment(1);
            match self.fill_queue(lanes) {
                None => return,
                Some(0) => self.wait_for_pending(&mut pending).await,
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
            let arrived = tx.timestamp;
            let priority = ordering.priority(&tx.transaction, base_fee);
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

    /// Moves transactions from `lanes` into the destination queue until it is full or `lanes` is
    /// empty, alternating lanes per [`LaneMix`]. Returns how many were queued, or `None` if the
    /// forwarder is gone.
    fn fill_queue(&mut self, lanes: &mut Snapshot<P::Transaction>) -> Option<u64> {
        let mut txs_sent: u64 = 0;
        while self.sender.capacity() > 0 {
            let Some(tx) = lanes.pop(self.lane_mix.next_lane()) else { break };
            let hash = *tx.hash();
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

    /// Discards queued pending-transaction announcements without waiting.
    ///
    /// Drops the listener once the pool closes it, so later waits fall back to the poll interval
    /// instead of spinning on a closed channel.
    fn drain_announcements(pending: &mut Option<mpsc::Receiver<TxHash>>) {
        let Some(receiver) = pending else { return };
        loop {
            match receiver.try_recv() {
                Ok(_) => {}
                Err(mpsc::error::TryRecvError::Empty) => return,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    *pending = None;
                    return;
                }
            }
        }
    }

    /// Waits until the pool announces a pending transaction, the poll interval elapses, or the
    /// reader is cancelled, whichever comes first.
    ///
    /// An announcement may concern a transaction the last snapshot already covered; the cost of
    /// such a spurious wake is one extra snapshot.
    async fn wait_for_pending(&self, pending: &mut Option<mpsc::Receiver<TxHash>>) {
        let announced = async {
            match pending {
                Some(receiver) => {
                    if receiver.recv().await.is_none() {
                        *pending = None;
                        time::sleep(self.config.poll_interval).await;
                    }
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = announced => {}
            () = time::sleep(self.config.poll_interval) => {}
            () = self.cancel.cancelled() => {}
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
    use alloy_consensus::transaction::Recovered;
    use alloy_primitives::{Address, B256, TxKind, U256};
    use base_common_consensus::{BaseTransactionSigned, TxDeposit};
    use base_execution_txpool::BasePooledTransaction;
    use reth_transaction_pool::{
        TransactionOrigin, identifier::TransactionId, noop::NoopTransactionPool,
    };

    use super::*;

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
        time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        time::timeout(Duration::from_secs(1), run).await.expect("reader exits").unwrap();
        announcer.join().unwrap();

        let mut spans = Vec::new();
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

    /// Cancelling while the reader waits out the gap after an expensive snapshot must end the
    /// reader at once rather than after the gap or another snapshot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_during_the_pacing_gap_returns_promptly() {
        const COST: Duration = Duration::from_millis(300);
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
        time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();

        time::timeout(Duration::from_millis(100), run)
            .await
            .expect("reader exits well before the gap or another snapshot ends")
            .unwrap();
    }
}
