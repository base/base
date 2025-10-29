use alloy_primitives::{Bytes, TxHash, b256, bytes};
use std::sync::Arc;
use tips_audit::{
    reader::Event,
    storage::{BundleEventS3Reader, EventWriter, S3EventReaderWriter},
    types::BundleEvent,
};
use tips_core::Bundle;
use tokio::task::JoinSet;
use uuid::Uuid;

mod common;
use common::TestHarness;

// https://basescan.org/tx/0x4f7ddfc911f5cf85dd15a413f4cbb2a0abe4f1ff275ed13581958c0bcf043c5e
const TXN_DATA: Bytes = bytes!(
    "0x02f88f8221058304b6b3018315fb3883124f80948ff2f0a8d017c79454aa28509a19ab9753c2dd1480a476d58e1a0182426068c9ea5b00000000000000000002f84f00000000083e4fda54950000c080a086fbc7bbee41f441fb0f32f7aa274d2188c460fe6ac95095fa6331fa08ec4ce7a01aee3bcc3c28f7ba4e0c24da9ae85e9e0166c73cabb42c25ff7b5ecd424f3105"
);
const TXN_HASH: TxHash =
    b256!("0x4f7ddfc911f5cf85dd15a413f4cbb2a0abe4f1ff275ed13581958c0bcf043c5e");

fn create_test_bundle() -> Bundle {
    Bundle {
        txs: vec![TXN_DATA.clone()],
        ..Default::default()
    }
}

fn create_test_event(key: &str, timestamp: i64, bundle_event: BundleEvent) -> Event {
    Event {
        key: key.to_string(),
        timestamp,
        event: bundle_event,
    }
}

#[tokio::test]
async fn test_event_write_and_read() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let bundle_id = Uuid::new_v4();
    let bundle = create_test_bundle();
    let event = create_test_event(
        "test-key-1",
        1234567890,
        BundleEvent::Received {
            bundle_id,
            bundle: bundle.clone(),
        },
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

    let bundle_id_two = Uuid::new_v4();
    let bundle = create_test_bundle();
    let event = create_test_event(
        "test-key-2",
        1234567890,
        BundleEvent::Received {
            bundle_id: bundle_id_two,
            bundle: bundle.clone(),
        },
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
async fn test_events_appended() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let bundle_id = Uuid::new_v4();
    let bundle = create_test_bundle();

    let events = vec![
        create_test_event(
            "test-key-1",
            1234567890,
            BundleEvent::Received {
                bundle_id,
                bundle: bundle.clone(),
            },
        ),
        create_test_event(
            "test-key-2",
            1234567891,
            BundleEvent::Cancelled { bundle_id },
        ),
    ];

    for (idx, event) in events.iter().enumerate() {
        writer.archive_event(event.clone()).await?;

        let bundle_history = writer.get_bundle_history(bundle_id).await?;
        assert!(bundle_history.is_some());

        let history = bundle_history.unwrap();
        assert_eq!(history.history.len(), idx + 1);

        let keys: Vec<String> = history
            .history
            .iter()
            .map(|e| e.key().to_string())
            .collect();
        assert_eq!(
            keys,
            events
                .iter()
                .map(|e| e.key.clone())
                .take(idx + 1)
                .collect::<Vec<String>>()
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_event_deduplication() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let bundle_id = Uuid::new_v4();
    let bundle = create_test_bundle();
    let event = create_test_event(
        "duplicate-key",
        1234567890,
        BundleEvent::Received {
            bundle_id,
            bundle: bundle.clone(),
        },
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
async fn test_nonexistent_data() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let nonexistent_bundle_id = Uuid::new_v4();
    let bundle_history = writer.get_bundle_history(nonexistent_bundle_id).await?;
    assert!(bundle_history.is_none());

    let nonexistent_tx_hash = TxHash::from([255u8; 32]);
    let metadata = writer.get_transaction_metadata(nonexistent_tx_hash).await?;
    assert!(metadata.is_none());

    Ok(())
}

#[tokio::test]
#[ignore = "TODO doesn't appear to work with minio, should test against a real S3 bucket"]
async fn test_concurrent_writes_for_bundle() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
{
    let harness = TestHarness::new().await?;
    let writer = Arc::new(S3EventReaderWriter::new(
        harness.s3_client.clone(),
        harness.bucket_name.clone(),
    ));

    let bundle_id = Uuid::new_v4();
    let bundle = create_test_bundle();

    let event = create_test_event(
        "hello-dan",
        1234567889i64,
        BundleEvent::Received {
            bundle_id,
            bundle: bundle.clone(),
        },
    );

    writer.archive_event(event.clone()).await?;

    let mut join_set = JoinSet::new();

    for i in 0..4 {
        let writer_clone = writer.clone();
        let key = if i % 4 == 0 {
            "shared-key".to_string()
        } else {
            format!("unique-key-{i}")
        };

        let event = create_test_event(
            &key,
            1234567890 + i as i64,
            BundleEvent::Received {
                bundle_id,
                bundle: bundle.clone(),
            },
        );

        join_set.spawn(async move { writer_clone.archive_event(event.clone()).await });
    }

    let tasks = join_set.join_all().await;
    assert_eq!(tasks.len(), 4);
    for t in tasks.iter() {
        assert!(t.is_ok());
    }

    let bundle_history = writer.get_bundle_history(bundle_id).await?;
    assert!(bundle_history.is_some());

    let history = bundle_history.unwrap();

    let shared_count = history
        .history
        .iter()
        .filter(|e| e.key() == "shared-key")
        .count();
    assert_eq!(shared_count, 1);

    let unique_count = history
        .history
        .iter()
        .filter(|e| e.key().starts_with("unique-key-"))
        .count();
    assert_eq!(unique_count, 3);

    assert_eq!(history.history.len(), 4);

    Ok(())
}
