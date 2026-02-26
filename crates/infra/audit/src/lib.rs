#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/tips/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod archiver;
pub use archiver::KafkaAuditArchiver;

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

mod storage;
pub use storage::{
    BundleEventS3Reader, BundleHistory, BundleHistoryEvent, EventWriter, S3EventReaderWriter,
    S3Key, TransactionMetadata,
};

mod types;
use tokio::sync::mpsc;
use tracing::error;
pub use types::{BundleEvent, BundleId, DropReason, Transaction, TransactionId};

/// Connects bundle event receivers to publishers.
#[derive(Debug)]
pub struct AuditConnector;

impl AuditConnector {
    /// Connects a bundle event receiver to a publisher, spawning a task to forward events.
    pub fn connect<P>(event_rx: mpsc::UnboundedReceiver<BundleEvent>, publisher: P)
    where
        P: BundleEventPublisher + 'static,
    {
        tokio::spawn(async move {
            let mut event_rx = event_rx;
            while let Some(event) = event_rx.recv().await {
                if let Err(e) = publisher.publish(event).await {
                    error!(error = %e, "failed to publish bundle event");
                }
            }
        });
    }
}
