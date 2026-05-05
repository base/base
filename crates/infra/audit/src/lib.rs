#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod archiver;
pub use archiver::AuditArchiver;

mod kafka_config;
pub use kafka_config::load_kafka_config_from_file;

mod metrics;
pub use metrics::Metrics;

mod publisher;
pub use publisher::{BundleEventPublisher, KafkaBundleEventPublisher, LoggingBundleEventPublisher};

mod reader;
pub use reader::{
    Event, EventReader, KafkaAuditLogReader, assign_topic_partition, create_kafka_consumer,
};

mod rpc;
pub use rpc::{AuditArchiverApiServer, AuditArchiverRpc};

mod rpc_publisher;
pub use rpc_publisher::{DEFAULT_RPC_TIMEOUT, RpcBundleEventPublisher};

mod rpc_reader;
pub use rpc_reader::RpcEventReader;

mod storage;
pub use storage::{
    BundleEventS3Reader, BundleHistory, BundleHistoryEvent, EventWriter, S3EventReaderWriter,
    S3Key, TransactionMetadata,
};

mod types;
use core::time::Duration;

use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};
use tracing::{error, trace};
pub use types::{BundleEvent, BundleId, DropReason, Transaction, TransactionId};

/// Connects bundle event receivers to publishers.
#[derive(Debug)]
pub struct AuditConnector;

impl AuditConnector {
    /// Connects a bundle event receiver to a publisher, batching events and
    /// forwarding them in groups via [`BundleEventPublisher::publish_all`].
    ///
    /// The batching policy is "deadline per batch": when the first event of an
    /// otherwise-empty buffer arrives, a deadline of `now + batch_max_wait` is
    /// established. The buffer is flushed when either:
    ///
    /// - the buffer reaches `batch_max_size`, or
    /// - the deadline elapses with at least one buffered event.
    ///
    /// On flush, the deadline is dropped and the next incoming event starts a
    /// fresh deadline. When `event_rx` is closed, any remaining buffered events
    /// are flushed before the spawned task exits.
    ///
    /// Publish failures are logged and the offending batch is dropped; the
    /// connector does not retry and does not apply backpressure to `event_rx`.
    pub fn connect_batched<P>(
        event_rx: mpsc::Receiver<BundleEvent>,
        publisher: P,
        batch_max_size: usize,
        batch_max_wait: Duration,
    ) where
        P: BundleEventPublisher + 'static,
    {
        tokio::spawn(async move {
            let mut event_rx = event_rx;
            let mut buffer: Vec<BundleEvent> = Vec::with_capacity(batch_max_size);
            let mut deadline: Option<Instant> = None;

            loop {
                let recv_result = match deadline {
                    Some(d) => {
                        tokio::select! {
                            maybe_event = event_rx.recv() => maybe_event,
                            () = sleep_until(d) => {
                                Self::flush(&publisher, &mut buffer).await;
                                deadline = None;
                                continue;
                            }
                        }
                    }
                    None => event_rx.recv().await,
                };

                match recv_result {
                    Some(event) => {
                        buffer.push(event);
                        if deadline.is_none() {
                            deadline = Some(Instant::now() + batch_max_wait);
                        }
                        if buffer.len() >= batch_max_size {
                            Self::flush(&publisher, &mut buffer).await;
                            deadline = None;
                        }
                    }
                    None => {
                        // Channel closed: flush any remaining events and exit.
                        if !buffer.is_empty() {
                            Self::flush(&publisher, &mut buffer).await;
                        }
                        break;
                    }
                }
            }
        });
    }

    /// Drains `buffer` and ships it via `publisher.publish_all`. Errors are
    /// logged and swallowed; the batch is dropped.
    async fn flush<P>(publisher: &P, buffer: &mut Vec<BundleEvent>)
    where
        P: BundleEventPublisher,
    {
        if buffer.is_empty() {
            return;
        }
        let batch: Vec<BundleEvent> = std::mem::take(buffer);
        let batch_size = batch.len();
        match publisher.publish_all(batch).await {
            Ok(()) => trace!(batch_size, "Flushed bundle event batch"),
            Err(e) => {
                error!(
                    error = %e,
                    batch_size,
                    "Failed to publish bundle event batch; batch dropped"
                );
                Metrics::rpc_publish_failures("rpc_error").increment(1);
            }
        }
    }
}
