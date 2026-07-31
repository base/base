use std::{fmt, sync::Arc};

use alloy_primitives::TxHash;
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use reth_transaction_pool::{PoolTransaction, TransactionPool, ValidPoolTransaction};
use serde_json::{Map, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace};
use url::Url;

use super::{config::ConsumerConfig, metrics::Metrics, validator::RecentlySent};

/// Background consumer that drains the pool for one destination.
///
/// Each iteration creates a fresh `best_transactions()` snapshot and queues
/// transactions not recently accepted by this destination's queue.
pub(crate) struct DestinationConsumer<P: TransactionPool> {
    pool: P,
    config: ConsumerConfig,
    recently_sent: RecentlySent,
    sender: mpsc::Sender<Arc<ValidPoolTransaction<P::Transaction>>>,
    cancel: CancellationToken,
    builder_url: Url,
    url_label: Arc<str>,
}

impl<P> DestinationConsumer<P>
where
    P: TransactionPool + 'static,
    P::Transaction: PoolTransaction,
{
    /// Creates a consumer for one destination.
    pub(crate) fn new(
        pool: P,
        config: ConsumerConfig,
        sender: mpsc::Sender<Arc<ValidPoolTransaction<P::Transaction>>>,
        cancel: CancellationToken,
        builder_url: Url,
    ) -> Self {
        let recently_sent = RecentlySent::new(config.resend_after);
        let url_label = builder_url.to_string().into();
        Self { pool, config, recently_sent, sender, cancel, builder_url, url_label }
    }

    /// Blocking loop — runs until the [`CancellationToken`] is cancelled.
    pub(crate) fn run(&mut self) {
        info!(
            builder_url = %self.builder_url,
            resend_after_ms = self.config.resend_after.as_millis() as u64,
            channel_capacity = self.config.channel_capacity,
            poll_interval_ms = self.config.poll_interval.as_millis() as u64,
            "starting transaction consumer",
        );

        while !self.cancel.is_cancelled() {
            let mut txs_read: u64 = 0;
            let mut txs_sent: u64 = 0;
            let mut txs_ignored: u64 = 0;

            let best_txs = self.pool.best_transactions();

            for tx in best_txs {
                if self.cancel.is_cancelled() {
                    info!("consumer cancelled during iteration");
                    return;
                }

                let iterator_index = txs_read;
                txs_read += 1;
                let hash = *tx.hash();

                if self.recently_sent.was_recently_sent(&hash) {
                    txs_ignored += 1;
                    continue;
                }

                let mut pending = tx;
                loop {
                    match self.sender.try_send(pending) {
                        Ok(()) => {
                            self.recently_sent.mark_sent(hash);
                            txs_sent += 1;
                            self.emit_builder_consumed_event(hash, iterator_index);
                            break;
                        }
                        Err(mpsc::error::TrySendError::Full(tx)) => {
                            if self.cancel.is_cancelled() {
                                return;
                            }
                            pending = tx;
                            std::thread::sleep(self.config.poll_interval);
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }

            Metrics::iterations(Arc::clone(&self.url_label)).increment(1);

            if txs_read > 0 {
                Metrics::txs_read(Arc::clone(&self.url_label)).increment(txs_read);
                Metrics::txs_sent(Arc::clone(&self.url_label)).increment(txs_sent);
                Metrics::txs_ignored(Arc::clone(&self.url_label)).increment(txs_ignored);
                Metrics::dedup_cache_size(Arc::clone(&self.url_label))
                    .set(self.recently_sent.len() as f64);

                trace!(
                    builder_url = %self.builder_url,
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

    fn emit_builder_consumed_event(&self, tx_hash: TxHash, iterator_index: u64) {
        let event_type = TransactionEventType::TxpoolBuilderConsumed;
        let data = Map::from_iter([
            ("source".to_string(), json!("best_transactions")),
            ("target".to_string(), json!("builder_forwarder")),
            ("builder_url".to_string(), json!(self.builder_url.as_str())),
            ("iterator_index".to_string(), json!(iterator_index)),
            ("resend_after_ms".to_string(), json!(self.config.resend_after.as_millis() as u64)),
        ]);
        let _ = transaction_event!(
            producer: TransactionEventProducer::BaseRethNode,
            event_type: event_type,
            tx_hash: tx_hash,
            id: {
                "tx_hash" => format!("{tx_hash:#x}"),
                "iterator_index" => iterator_index,
            },
            data: data,
        );
    }
}

impl<P: TransactionPool> fmt::Debug for DestinationConsumer<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DestinationConsumer")
            .field("builder_url", &self.builder_url)
            .field("config", &self.config)
            .field("recently_sent", &self.recently_sent)
            .finish_non_exhaustive()
    }
}
