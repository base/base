use std::{fmt, sync::Arc};

use alloy_eips::Encodable2718;
use alloy_primitives::{Bytes, TxHash};
use base_execution_txpool::{NoExtensions, ValidatedTransaction, ValidatedTransactionExtensions};
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use reth_transaction_pool::{PoolTransaction, TransactionPool, ValidPoolTransaction};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace};
use url::Url;

use super::{config::ReaderConfig, metrics::Metrics, validator::RecentlySent};
use crate::forwarder::InsertValidatedTransaction;

/// Background reader that drains the pool for one destination.
///
/// Each iteration creates a fresh `best_transactions()` snapshot and queues
/// transactions not recently accepted by this destination's queue.
///
/// Conversion to the builder-RPC wire form happens here rather than in the
/// forwarder: this is the component that holds a [`ValidPoolTransaction`], and
/// it runs on a blocking thread, which keeps the EIP-2718 encoding off the
/// async runtime.
pub(crate) struct DestinationReader<P: TransactionPool, E = NoExtensions> {
    pool: P,
    config: ReaderConfig,
    recently_sent: RecentlySent,
    sender: mpsc::Sender<InsertValidatedTransaction<E>>,
    cancel: CancellationToken,
    builder_url: Url,
    url_label: Arc<str>,
}

impl<P, E> DestinationReader<P, E>
where
    P: TransactionPool + 'static,
    P::Transaction: PoolTransaction,
    <P::Transaction as PoolTransaction>::Consensus: Encodable2718,
    E: ValidatedTransactionExtensions<P::Transaction>,
{
    /// Creates a reader for one destination.
    pub(crate) fn new(
        pool: P,
        config: ReaderConfig,
        sender: mpsc::Sender<InsertValidatedTransaction<E>>,
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
            "starting transaction reader",
        );

        while !self.cancel.is_cancelled() {
            let mut txs_read: u64 = 0;
            let mut txs_sent: u64 = 0;
            let mut txs_ignored: u64 = 0;

            let best_txs = self.pool.best_transactions();

            let mut queue_full = false;
            for tx in best_txs {
                if self.cancel.is_cancelled() {
                    info!("reader cancelled during iteration");
                    return;
                }

                let iterator_index = txs_read;
                txs_read += 1;
                let hash = *tx.hash();

                if self.recently_sent.was_recently_sent(&hash) {
                    txs_ignored += 1;
                    continue;
                }

                match self.try_enqueue(&tx) {
                    Ok(()) => {
                        self.recently_sent.mark_sent(hash);
                        txs_sent += 1;
                        self.emit_builder_consumed_event(hash, iterator_index);
                    }
                    Err(mpsc::error::TrySendError::Full(())) => {
                        queue_full = true;
                        break;
                    }
                    Err(mpsc::error::TrySendError::Closed(())) => return,
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
                    "reader iteration complete",
                );
            }

            if queue_full || txs_sent == 0 {
                std::thread::sleep(self.config.poll_interval);
            }
        }

        info!("reader cancelled, shutting down");
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
    /// Done once per transaction rather than once per queue-full retry: the encoding is the
    /// expensive part and the result is what the queue carries.
    fn to_wire(
        transaction: &Arc<ValidPoolTransaction<P::Transaction>>,
    ) -> InsertValidatedTransaction<E> {
        let consensus = transaction.transaction.clone_into_consensus();
        InsertValidatedTransaction {
            transaction: ValidatedTransaction {
                sender: *transaction.sender_ref(),
                raw: Bytes::from(consensus.inner().encoded_2718()),
                extensions: E::extract(transaction),
            },
            tx_hash: *transaction.transaction.hash(),
        }
    }

    fn emit_builder_consumed_event(&self, tx_hash: TxHash, iterator_index: u64) {
        let _ = transaction_event!(
            producer: TransactionEventProducer::BaseRethNode,
            event_type: TransactionEventType::TxpoolBuilderConsumed,
            tx_hash: tx_hash,
            id: {
                "tx_hash" => format!("{tx_hash:#x}"),
                "iterator_index" => iterator_index,
            },
            data: {
                "source" => "best_transactions",
                "target" => "builder_forwarder",
                "builder_url" => self.builder_url.as_str(),
                "iterator_index" => iterator_index,
                "resend_after_ms" => self.config.resend_after.as_millis() as u64,
            },
        );
    }
}

impl<P: TransactionPool, E> fmt::Debug for DestinationReader<P, E> {
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
    use std::time::{Duration, Instant};

    use alloy_consensus::transaction::Recovered;
    use alloy_primitives::{Address, B256, TxKind, U256};
    use base_common_consensus::{BaseTransactionSigned, TxDeposit};
    use base_execution_txpool::BasePooledTransaction;
    use reth_transaction_pool::{
        TransactionOrigin, identifier::TransactionId, noop::NoopTransactionPool,
    };

    use super::*;

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
        DestinationReader::new(
            NoopTransactionPool::new(),
            ReaderConfig {
                resend_after: Duration::from_secs(4),
                channel_capacity: 1,
                poll_interval: Duration::from_millis(1),
            },
            sender,
            cancel,
            "http://builder.test".parse().unwrap(),
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
}
