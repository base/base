//! Audit library for tracking and archiving bundle and user operation events.
//!
//! This crate provides functionality for publishing events to Kafka,
//! archiving them to S3, and reading event history.

#![doc(issue_tracker_base_url = "https://github.com/base/tips/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod archiver;
pub use archiver::KafkaAuditArchiver;

mod metrics;
pub use metrics::Metrics;

mod publisher;
pub use publisher::{
    BundleEventPublisher, KafkaBundleEventPublisher, KafkaUserOpEventPublisher,
    LoggingBundleEventPublisher, LoggingUserOpEventPublisher, UserOpEventPublisher,
};

mod reader;
pub use reader::{
    Event, EventReader, KafkaAuditLogReader, KafkaUserOpAuditLogReader, UserOpEventReader,
    UserOpEventWrapper, assign_topic_partition, create_kafka_consumer,
};

mod storage;
pub use storage::{
    BundleEventS3Reader, BundleHistory, BundleHistoryEvent, EventWriter, S3EventReaderWriter,
    S3Key, TransactionMetadata, UserOpEventS3Reader, UserOpEventWriter, UserOpHistory,
    UserOpHistoryEvent,
};

mod types;
pub use types::{
    BundleEvent, BundleId, DropReason, Transaction, TransactionId, UserOpDropReason, UserOpEvent,
    UserOpHash,
};

use tokio::sync::mpsc;
use tracing::error;

/// Connects a bundle event receiver to a publisher, spawning a task to forward events.
pub fn connect_audit_to_publisher<P>(event_rx: mpsc::UnboundedReceiver<BundleEvent>, publisher: P)
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

/// Connects a user operation event receiver to a publisher, spawning a task to forward events.
pub fn connect_userop_audit_to_publisher<P>(
    event_rx: mpsc::UnboundedReceiver<UserOpEvent>,
    publisher: P,
) where
    P: UserOpEventPublisher + 'static,
{
    tokio::spawn(async move {
        let mut event_rx = event_rx;
        while let Some(event) = event_rx.recv().await {
            if let Err(e) = publisher.publish(event).await {
                error!(error = %e, "Failed to publish user op event");
            }
        }
    });
}
