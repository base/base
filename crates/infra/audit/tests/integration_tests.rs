//! Integration tests for the Kafka publisher and S3 archiver pipeline.

use std::time::Duration;

use audit_archiver_lib::{
    BundleEvent, BundleEventPublisher, BundleEventS3Reader, DropReason, KafkaAuditArchiver,
    KafkaAuditLogReader, KafkaBundleEventPublisher, S3EventReaderWriter,
};
use base_primitives::{BundleExtensions, create_bundle_from_txn_data};
use uuid::Uuid;
mod common;
use common::TestHarness;

#[tokio::test]
#[ignore = "TODO doesn't appear to work with minio, should test against a real S3 bucket"]
async fn system_test_kafka_publisher_s3_archiver_integration() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    let topic = "test-mempool-events";

    let s3_writer =
        S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let bundle = create_bundle_from_txn_data();
    let test_bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
    let test_events = [
        BundleEvent::Received { bundle_id: test_bundle_id, bundle: Box::new(bundle.clone()) },
        BundleEvent::Dropped { bundle_id: test_bundle_id, reason: DropReason::TimedOut },
    ];

    let publisher = KafkaBundleEventPublisher::new(harness.kafka_producer, topic.to_string());

    for event in &test_events {
        publisher.publish(event.clone()).await?;
    }

    let mut consumer = KafkaAuditArchiver::new(
        KafkaAuditLogReader::new(harness.kafka_consumer, topic.to_string())?,
        s3_writer.clone(),
        1,
        100,
        false,
    );

    tokio::spawn(async move {
        consumer.run().await.expect("error running consumer");
    });

    // Wait for the messages to be received
    let mut counter = 0;
    loop {
        counter += 1;
        if counter > 10 {
            panic!("unable to complete archiving within the deadline");
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
        let bundle_history = s3_writer.get_bundle_history(test_bundle_id).await?;

        if let Some(history) = bundle_history {
            if history.history.len() == test_events.len() {
                break;
            }
            continue;
        }
        continue;
    }

    Ok(())
}
