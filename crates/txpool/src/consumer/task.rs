use std::{fmt, sync::Arc};

use reth_transaction_pool::{PoolTransaction, TransactionPool, ValidPoolTransaction};
use tokio::sync::broadcast;
use tracing::{info, trace};

use super::{config::ConsumerConfig, metrics::ConsumerMetrics, validator::RecentlySent};

/// Background consumer that drains the pool and broadcasts transactions.
///
/// Runs on a dedicated blocking thread via [`tokio::task::spawn_blocking`].
/// Each iteration creates a fresh `best_transactions()` snapshot, skips
/// recently-sent hashes, and broadcasts new transactions. Downstream
/// forwarders (one per builder) each subscribe to receive every transaction.
pub struct Consumer<P: TransactionPool> {
    pool: P,
    config: ConsumerConfig,
    recently_sent: RecentlySent,
    sender: broadcast::Sender<Arc<ValidPoolTransaction<P::Transaction>>>,
    metrics: ConsumerMetrics,
}

impl<P> Consumer<P>
where
    P: TransactionPool + 'static,
    P::Transaction: PoolTransaction,
{
    /// Creates a new consumer.
    pub fn new(
        pool: P,
        config: ConsumerConfig,
        sender: broadcast::Sender<Arc<ValidPoolTransaction<P::Transaction>>>,
        metrics: ConsumerMetrics,
    ) -> Self {
        let recently_sent = RecentlySent::new(config.resend_after);
        Self { pool, config, recently_sent, sender, metrics }
    }

    /// Blocking loop — intended to be called from [`tokio::task::spawn_blocking`].
    ///
    /// Returns when all receivers have been dropped.
    pub fn run(self) {
        info!(
            resend_after_ms = self.config.resend_after.as_millis() as u64,
            channel_capacity = self.config.channel_capacity,
            poll_interval_ms = self.config.poll_interval.as_millis() as u64,
            "starting transaction consumer",
        );

        loop {
            let mut txs_read: u64 = 0;
            let mut txs_sent: u64 = 0;
            let mut txs_ignored: u64 = 0;

            let best_txs = self.pool.best_transactions();

            for tx in best_txs {
                txs_read += 1;
                let hash = *tx.hash();

                if self.recently_sent.was_recently_sent(&hash) {
                    txs_ignored += 1;
                    continue;
                }

                match self.sender.send(tx) {
                    Ok(_receivers) => {
                        self.recently_sent.mark_sent(hash);
                        txs_sent += 1;
                    }
                    Err(_) => {
                        info!("all broadcast receivers dropped, shutting down");
                        return;
                    }
                }
            }

            if txs_read > 0 {
                self.metrics.txs_read.increment(txs_read);
                self.metrics.txs_sent.increment(txs_sent);
                self.metrics.txs_ignored.increment(txs_ignored);
                self.metrics.dedup_cache_size.set(self.recently_sent.len() as f64);

                trace!(
                    txs_read = txs_read,
                    txs_sent = txs_sent,
                    txs_ignored = txs_ignored,
                    dedup_cache = self.recently_sent.len(),
                    "consumer iteration complete",
                );
            }

            if txs_read == 0 {
                std::thread::sleep(self.config.poll_interval);
            }
        }
    }
}

impl<P: TransactionPool> fmt::Debug for Consumer<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Consumer")
            .field("config", &self.config)
            .field("recently_sent", &self.recently_sent)
            .finish_non_exhaustive()
    }
}
