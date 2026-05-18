use std::{fmt, sync::Arc};

use reth_transaction_pool::{EthPoolTransaction, TransactionPool, ValidPoolTransaction};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace};

use super::{config::ConsumerConfig, metrics::Metrics, validator::RecentlySent};

/// Background consumer that drains the pool and broadcasts transactions.
///
/// Each iteration creates a fresh `best_transactions()` snapshot from the
/// pool, skips recently-sent hashes, and broadcasts new transactions.
/// Downstream forwarders (one per builder) each subscribe to receive every
/// transaction.
///
/// The pool's [`TransactionPool::best_transactions`] already merges the
/// standard pool and the EIP-8130 2D nonce side-pool when backed by
/// [`BaseTransactionPool`](crate::BaseTransactionPool), so this consumer
/// sees both standard and AA transactions transparently.
pub struct Consumer<P: TransactionPool> {
    pool: P,
    config: ConsumerConfig,
    recently_sent: RecentlySent,
    sender: broadcast::Sender<Arc<ValidPoolTransaction<P::Transaction>>>,
    cancel: CancellationToken,
}

impl<P> Consumer<P>
where
    P: TransactionPool + 'static,
    P::Transaction: EthPoolTransaction + Clone,
{
    /// Creates a new consumer.
    pub fn new(
        pool: P,
        config: ConsumerConfig,
        sender: broadcast::Sender<Arc<ValidPoolTransaction<P::Transaction>>>,
        cancel: CancellationToken,
    ) -> Self {
        let recently_sent = RecentlySent::new(config.resend_after);
        Self { pool, config, recently_sent, sender, cancel }
    }

    /// Blocking loop — runs until the [`CancellationToken`] is cancelled.
    pub fn run(&mut self) {
        info!(
            resend_after_ms = self.config.resend_after.as_millis() as u64,
            channel_capacity = self.config.channel_capacity,
            poll_interval_ms = self.config.poll_interval.as_millis() as u64,
            "starting transaction consumer",
        );

        while !self.cancel.is_cancelled() {
            let mut txs_read: u64 = 0;
            let mut txs_sent: u64 = 0;
            let mut txs_ignored: u64 = 0;

            for tx in self.pool.best_transactions() {
                if self.cancel.is_cancelled() {
                    info!("consumer cancelled during iteration");
                    return;
                }

                txs_read += 1;
                let hash = *tx.hash();

                if self.recently_sent.was_recently_sent(&hash) {
                    txs_ignored += 1;
                    continue;
                }

                if self.sender.send(tx).is_ok() {
                    self.recently_sent.mark_sent(hash);
                    txs_sent += 1;
                }
            }

            Metrics::iterations().increment(1);

            if txs_read > 0 {
                Metrics::txs_read().increment(txs_read);
                Metrics::txs_sent().increment(txs_sent);
                Metrics::txs_ignored().increment(txs_ignored);
                Metrics::dedup_cache_size().set(self.recently_sent.len() as f64);

                trace!(
                    txs_read = txs_read,
                    txs_sent = txs_sent,
                    txs_ignored = txs_ignored,
                    dedup_cache = self.recently_sent.len(),
                    "consumer iteration complete",
                );
            }

            if txs_sent == 0 {
                std::thread::sleep(self.config.poll_interval);
            }
        }

        info!("consumer cancelled, shutting down");
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
