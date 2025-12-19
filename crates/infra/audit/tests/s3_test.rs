use alloy_primitives::{Address, B256, TxHash, U256};
use std::sync::Arc;
use tips_audit::{
    reader::Event,
    storage::{
        BundleEventS3Reader, EventWriter, S3EventReaderWriter, UserOpEventS3Reader,
        UserOpEventWrapper, UserOpEventWriter,
    },
    types::{BundleEvent, UserOpDropReason, UserOpEvent},
};
use tokio::task::JoinSet;
use uuid::Uuid;

mod common;
use common::TestHarness;
use tips_core::test_utils::{TXN_HASH, create_bundle_from_txn_data};

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
    let bundle = create_bundle_from_txn_data();
    let event = create_test_event(
        "test-key-1",
        1234567890,
        BundleEvent::Received {
            bundle_id,
            bundle: Box::new(bundle.clone()),
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
    let bundle = create_bundle_from_txn_data();
    let event = create_test_event(
        "test-key-2",
        1234567890,
        BundleEvent::Received {
            bundle_id: bundle_id_two,
            bundle: Box::new(bundle.clone()),
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
    let bundle = create_bundle_from_txn_data();

    let events = [
        create_test_event(
            "test-key-1",
            1234567890,
            BundleEvent::Received {
                bundle_id,
                bundle: Box::new(bundle.clone()),
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
    let bundle = create_bundle_from_txn_data();
    let event = create_test_event(
        "duplicate-key",
        1234567890,
        BundleEvent::Received {
            bundle_id,
            bundle: Box::new(bundle.clone()),
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
    let bundle = create_bundle_from_txn_data();

    let event = create_test_event(
        "hello-dan",
        1234567889i64,
        BundleEvent::Received {
            bundle_id,
            bundle: Box::new(bundle.clone()),
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
                bundle: Box::new(bundle.clone()),
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

fn create_test_userop_event(
    key: &str,
    timestamp: i64,
    userop_event: UserOpEvent,
) -> UserOpEventWrapper {
    UserOpEventWrapper {
        key: key.to_string(),
        timestamp,
        event: userop_event,
    }
}

#[tokio::test]
async fn test_userop_event_write_and_read() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
{
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let user_op_hash = B256::from([1u8; 32]);
    let sender = Address::from([2u8; 20]);
    let entry_point = Address::from([3u8; 20]);
    let nonce = U256::from(1);

    let event = create_test_userop_event(
        "test-userop-key-1",
        1234567890,
        UserOpEvent::AddedToMempool {
            user_op_hash,
            sender,
            entry_point,
            nonce,
        },
    );

    writer.archive_userop_event(event).await?;

    let userop_history = writer.get_userop_history(user_op_hash).await?;
    assert!(userop_history.is_some());

    let history = userop_history.unwrap();
    assert_eq!(history.history.len(), 1);
    assert_eq!(history.history[0].key(), "test-userop-key-1");

    Ok(())
}

#[tokio::test]
async fn test_userop_events_appended() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let user_op_hash = B256::from([10u8; 32]);
    let sender = Address::from([11u8; 20]);
    let entry_point = Address::from([12u8; 20]);
    let nonce = U256::from(1);
    let tx_hash = TxHash::from([13u8; 32]);

    let events = [
        create_test_userop_event(
            "userop-key-1",
            1234567890,
            UserOpEvent::AddedToMempool {
                user_op_hash,
                sender,
                entry_point,
                nonce,
            },
        ),
        create_test_userop_event(
            "userop-key-2",
            1234567891,
            UserOpEvent::Included {
                user_op_hash,
                block_number: 12345,
                tx_hash,
            },
        ),
    ];

    for (idx, event) in events.iter().enumerate() {
        writer.archive_userop_event(event.clone()).await?;

        let userop_history = writer.get_userop_history(user_op_hash).await?;
        assert!(userop_history.is_some());

        let history = userop_history.unwrap();
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
async fn test_userop_event_deduplication() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let user_op_hash = B256::from([20u8; 32]);
    let sender = Address::from([21u8; 20]);
    let entry_point = Address::from([22u8; 20]);
    let nonce = U256::from(1);

    let event = create_test_userop_event(
        "duplicate-userop-key",
        1234567890,
        UserOpEvent::AddedToMempool {
            user_op_hash,
            sender,
            entry_point,
            nonce,
        },
    );

    writer.archive_userop_event(event.clone()).await?;
    writer.archive_userop_event(event).await?;

    let userop_history = writer.get_userop_history(user_op_hash).await?;
    assert!(userop_history.is_some());

    let history = userop_history.unwrap();
    assert_eq!(history.history.len(), 1);
    assert_eq!(history.history[0].key(), "duplicate-userop-key");

    Ok(())
}

#[tokio::test]
async fn test_userop_nonexistent_returns_none()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let nonexistent_hash = B256::from([255u8; 32]);
    let userop_history = writer.get_userop_history(nonexistent_hash).await?;
    assert!(userop_history.is_none());

    Ok(())
}

#[tokio::test]
async fn test_userop_all_event_types() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let user_op_hash = B256::from([30u8; 32]);
    let sender = Address::from([31u8; 20]);
    let entry_point = Address::from([32u8; 20]);
    let nonce = U256::from(1);
    let tx_hash = TxHash::from([33u8; 32]);

    let event1 = create_test_userop_event(
        "event-added",
        1234567890,
        UserOpEvent::AddedToMempool {
            user_op_hash,
            sender,
            entry_point,
            nonce,
        },
    );
    writer.archive_userop_event(event1).await?;

    let event2 = create_test_userop_event(
        "event-included",
        1234567891,
        UserOpEvent::Included {
            user_op_hash,
            block_number: 12345,
            tx_hash,
        },
    );
    writer.archive_userop_event(event2).await?;

    let userop_history = writer.get_userop_history(user_op_hash).await?;
    assert!(userop_history.is_some());

    let history = userop_history.unwrap();
    assert_eq!(history.history.len(), 2);
    assert_eq!(history.history[0].key(), "event-added");
    assert_eq!(history.history[1].key(), "event-included");

    Ok(())
}

#[tokio::test]
async fn test_userop_dropped_event() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let harness = TestHarness::new().await?;
    let writer = S3EventReaderWriter::new(harness.s3_client.clone(), harness.bucket_name.clone());

    let user_op_hash = B256::from([40u8; 32]);

    let event = create_test_userop_event(
        "event-dropped",
        1234567890,
        UserOpEvent::Dropped {
            user_op_hash,
            reason: UserOpDropReason::Invalid("AA21 didn't pay prefund".to_string()),
        },
    );
    writer.archive_userop_event(event).await?;

    let userop_history = writer.get_userop_history(user_op_hash).await?;
    assert!(userop_history.is_some());

    let history = userop_history.unwrap();
    assert_eq!(history.history.len(), 1);
    assert_eq!(history.history[0].key(), "event-dropped");

    Ok(())
}
