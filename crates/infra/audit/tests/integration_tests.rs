#![allow(missing_docs)]

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use tips_audit_lib::{
    BundleEvent, BundleEventPublisher, BundleEventS3Reader, DropReason, KafkaAuditArchiver,
    KafkaAuditLogReader, KafkaBundleEventPublisher, KafkaUserOpAuditLogReader,
    KafkaUserOpEventPublisher, S3EventReaderWriter, UserOpEvent, UserOpEventPublisher,
    UserOpEventReader,
};
use tips_core::{BundleExtensions, test_utils::create_bundle_from_txn_data};
use uuid::Uuid;
mod common;
use common::TestHarness;

#[tokio::test]
#[ignore = "TODO doesn't appear to work with minio, should test against a real S3 bucket"]
async fn test_kafka_publisher_s3_archiver_integration()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

#[tokio::test]
async fn test_userop_kafka_publisher_reader_integration()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let topic = "test-userop-events";

    let test_user_op_hash = B256::from_slice(&[1u8; 32]);
    let test_sender = Address::from_slice(&[2u8; 20]);
    let test_entry_point = Address::from_slice(&[3u8; 20]);
    let test_nonce = U256::from(42);

    let test_event = UserOpEvent::AddedToMempool {
        user_op_hash: test_user_op_hash,
        sender: test_sender,
        entry_point: test_entry_point,
        nonce: test_nonce,
    };

    let publisher = KafkaUserOpEventPublisher::new(harness.kafka_producer, topic.to_string());
    publisher.publish(test_event.clone()).await?;

    let mut reader = KafkaUserOpAuditLogReader::new(harness.kafka_consumer, topic.to_string())?;

    let received = tokio::time::timeout(Duration::from_secs(10), reader.read_event()).await??;

    assert_eq!(received.event.user_op_hash(), test_user_op_hash);

    match received.event {
        UserOpEvent::AddedToMempool { user_op_hash, sender, entry_point, nonce } => {
            assert_eq!(user_op_hash, test_user_op_hash);
            assert_eq!(sender, test_sender);
            assert_eq!(entry_point, test_entry_point);
            assert_eq!(nonce, test_nonce);
        }
        _ => panic!("Expected AddedToMempool event"),
    }

    reader.commit().await?;

    Ok(())
}
