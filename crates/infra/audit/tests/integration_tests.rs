use std::time::Duration;
use tips_audit::{
    KafkaAuditArchiver, KafkaAuditLogReader,
    publisher::{BundleEventPublisher, KafkaBundleEventPublisher},
    storage::{BundleEventS3Reader, S3EventReaderWriter},
    types::{BundleEvent, DropReason},
};
use tips_core::Bundle;
use uuid::Uuid;
mod common;
use common::TestHarness;

#[tokio::test]
async fn test_kafka_publisher_s3_archiver_integration()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let topic = "test-mempool-events";

    let s3_writer =
        S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let test_bundle_id = Uuid::new_v4();
    let test_events = vec![
        BundleEvent::Received {
            bundle_id: test_bundle_id,
            bundle: Bundle::default(),
        },
        BundleEvent::Dropped {
            bundle_id: test_bundle_id,
            reason: DropReason::TimedOut,
        },
    ];

    let publisher = KafkaBundleEventPublisher::new(harness.kafka_producer, topic.to_string());

    for event in test_events.iter() {
        publisher.publish(event.clone()).await?;
    }

    let mut consumer = KafkaAuditArchiver::new(
        KafkaAuditLogReader::new(harness.kafka_consumer, topic.to_string())?,
        s3_writer.clone(),
    );

    tokio::spawn(async move {
        consumer.run().await.expect("error running consumer");
    });

    // Wait for the messages to be received
    let mut counter = 0;
    loop {
        counter += 1;
        if counter > 10 {
            assert!(false, "unable to complete archiving within the deadline");
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
        let bundle_history = s3_writer.get_bundle_history(test_bundle_id).await?;

        if bundle_history.is_some() {
            let history = bundle_history.unwrap();
            if history.history.len() != test_events.len() {
                continue;
            } else {
                break;
            }
        } else {
            continue;
        }
    }

    Ok(())
}
