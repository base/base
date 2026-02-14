//! S3 event storage tests.

use std::sync::Arc;

use alloy_primitives::TxHash;
use tips_audit_lib::{BundleEvent, BundleEventS3Reader, Event, EventWriter, S3EventReaderWriter};
use tokio::task::JoinSet;
use uuid::Uuid;

mod common;
use base_primitives::{BundleExtensions, TXN_HASH, create_bundle_from_txn_data};
use common::TestHarness;

fn create_test_event(key: &str, timestamp: i64, bundle_event: BundleEvent) -> Event {
    Event { key: key.to_string(), timestamp, event: bundle_event }
}

#[tokio::test]
async fn system_test_event_write_and_read() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let bundle = create_bundle_from_txn_data();
    let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
    let event = create_test_event(
        "test-key-1",
        1234567890,
        BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) },
    );

    writer.archive_event(event).await?;

    let bundle_history = writer.get_bundle_history(bundle_id).await?;
    assert!(bundle_history.is_some());

    let history = bundle_history.unwrap();
    assert_eq!(history.history.len(), 1);
    assert_eq!(history.history[0].key(), "test-key-1");

    let metadata = writer.get_transaction_metadata(TXN_HASH).await?;
    assert!(metadata.is_some());

    if let Some(metadata) = metadata {
        assert!(metadata.bundle_ids.contains(&bundle_id));
    }

    let bundle_id_two = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
    let bundle = create_bundle_from_txn_data();
    let event = create_test_event(
        "test-key-2",
        1234567890,
        BundleEvent::Received { bundle_id: bundle_id_two, bundle: Box::new(bundle.clone()) },
    );

    writer.archive_event(event).await?;

    let metadata = writer.get_transaction_metadata(TXN_HASH).await?;
    assert!(metadata.is_some());

    if let Some(metadata) = metadata {
        assert!(metadata.bundle_ids.contains(&bundle_id));
        assert!(metadata.bundle_ids.contains(&bundle_id_two));
    }

    Ok(())
}

#[tokio::test]
async fn system_test_events_appended() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let bundle = create_bundle_from_txn_data();
    let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());

    let events = [
        create_test_event(
            "test-key-1",
            1234567890,
            BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) },
        ),
        create_test_event("test-key-2", 1234567891, BundleEvent::Cancelled { bundle_id }),
    ];

    for (idx, event) in events.iter().enumerate() {
        writer.archive_event(event.clone()).await?;

        let bundle_history = writer.get_bundle_history(bundle_id).await?;
        assert!(bundle_history.is_some());

        let history = bundle_history.unwrap();
        assert_eq!(history.history.len(), idx + 1);

        let keys: Vec<String> = history.history.iter().map(|e| e.key().to_string()).collect();
        assert_eq!(
            keys,
            events.iter().map(|e| e.key.clone()).take(idx + 1).collect::<Vec<String>>()
        );
    }

    Ok(())
}

#[tokio::test]
async fn system_test_event_deduplication() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let bundle = create_bundle_from_txn_data();
    let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
    let bundle = create_bundle_from_txn_data();
    let event = create_test_event(
        "duplicate-key",
        1234567890,
        BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) },
    );

    writer.archive_event(event.clone()).await?;
    writer.archive_event(event).await?;

    let bundle_history = writer.get_bundle_history(bundle_id).await?;
    assert!(bundle_history.is_some());

    let history = bundle_history.unwrap();
    assert_eq!(history.history.len(), 1);
    assert_eq!(history.history[0].key(), "duplicate-key");

    Ok(())
}

#[tokio::test]
async fn system_test_nonexistent_data() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let bundle = create_bundle_from_txn_data();
    let nonexistent_bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
    let bundle_history = writer.get_bundle_history(nonexistent_bundle_id).await?;
    assert!(bundle_history.is_none());

    let nonexistent_tx_hash = TxHash::from([255u8; 32]);
    let metadata = writer.get_transaction_metadata(nonexistent_tx_hash).await?;
    assert!(metadata.is_none());

    Ok(())
}

#[tokio::test]
#[ignore = "TODO doesn't appear to work with minio, should test against a real S3 bucket"]
async fn system_test_concurrent_writes_for_bundle() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    let writer =
        Arc::new(S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone()));

    let bundle = create_bundle_from_txn_data();
    let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());

    let event = create_test_event(
        "hello-dan",
        1234567889i64,
        BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) },
    );

    writer.archive_event(event.clone()).await?;

    let mut join_set: JoinSet<anyhow::Result<()>> = JoinSet::new();

    for i in 0..4 {
        let writer_clone = Arc::clone(&writer);
        let key = if i % 4 == 0 { "shared-key".to_string() } else { format!("unique-key-{i}") };

        let event = create_test_event(
            &key,
            1234567890 + i as i64,
            BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) },
        );

        join_set.spawn(async move { writer_clone.archive_event(event.clone()).await });
    }

    let tasks = join_set.join_all().await;
    assert_eq!(tasks.len(), 4);
    for t in &tasks {
        assert!(t.is_ok());
    }

    let bundle_history = writer.get_bundle_history(bundle_id).await?;
    assert!(bundle_history.is_some());

    let history = bundle_history.unwrap();

    let shared_count = history.history.iter().filter(|e| e.key() == "shared-key").count();
    assert_eq!(shared_count, 1);

    let unique_count =
        history.history.iter().filter(|e| e.key().starts_with("unique-key-")).count();
    assert_eq!(unique_count, 3);

    assert_eq!(history.history.len(), 4);

    Ok(())
}
