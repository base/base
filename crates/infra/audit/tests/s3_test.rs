//! S3 event storage tests.

use alloy_primitives::TxHash;
use audit_archiver_lib::{
    BundleEvent, BundleEventS3Reader, Event, EventWriter, S3EventReaderWriter,
};
use uuid::Uuid;

mod common;
use base_bundles::{
    BundleExtensions,
    test_utils::{TXN_HASH, create_bundle_from_txn_data},
};
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
    let bundle_hash = bundle.bundle_hash();
    let event = create_test_event(
        &format!("received-{bundle_hash}"),
        1234567890,
        BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) },
    );

    writer.archive_event(event).await?;

    let bundle_key = format!("{bundle_hash}");
    let bundle_history = writer.get_bundle_history(&bundle_key).await?;
    assert!(bundle_history.is_some(), "bundle history should exist after write");

    let history = bundle_history.unwrap();
    assert_eq!(history.history.len(), 1, "should have exactly 1 event");

    let metadata = writer.get_transaction_metadata(TXN_HASH).await?;
    assert!(metadata.is_some(), "transaction metadata should exist");

    if let Some(metadata) = metadata {
        let hash_str = format!("{bundle_hash}");
        assert!(metadata.bundle_ids.contains(&hash_str), "metadata should contain the bundle hash");
    }

    Ok(())
}

#[tokio::test]
async fn system_test_event_deduplication() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let bundle = create_bundle_from_txn_data();
    let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
    let bundle_hash = bundle.bundle_hash();
    let event = create_test_event(
        &format!("received-{bundle_hash}"),
        1234567890,
        BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) },
    );

    writer.archive_event(event.clone()).await?;
    writer.archive_event(event).await?;

    let bundle_key = format!("{bundle_hash}");
    let bundle_history = writer.get_bundle_history(&bundle_key).await?;
    assert!(bundle_history.is_some());

    let history = bundle_history.unwrap();
    assert_eq!(history.history.len(), 1, "duplicate write should not create a second event");

    Ok(())
}

#[tokio::test]
async fn system_test_nonexistent_data() -> anyhow::Result<()> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let nonexistent_key = format!("{}", TxHash::from([255u8; 32]));
    let bundle_history = writer.get_bundle_history(&nonexistent_key).await?;
    assert!(bundle_history.is_none());

    let nonexistent_tx_hash = TxHash::from([255u8; 32]);
    let metadata = writer.get_transaction_metadata(nonexistent_tx_hash).await?;
    assert!(metadata.is_none());

    Ok(())
}
