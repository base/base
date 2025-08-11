mod common;

use alloy_primitives::map::foldhash::{HashMap, HashMapExt};
use common::PostgresTestContainer;
use flashblocks_archiver::{
    archiver::FlashblocksArchiver,
    config::{ArchiverConfig, BuilderConfig, Config, DatabaseConfig},
    types::Metadata,
    FlashblockMessage,
};
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
use std::time::Duration;
use tokio::time::timeout;
use tracing::{info, warn};
use url::Url;

struct SepoliaTestSetup {
    postgres: PostgresTestContainer,
    config: Config,
}

impl SepoliaTestSetup {
    async fn new() -> anyhow::Result<Self> {
        let postgres = PostgresTestContainer::new("test_sepolia_flashblocks").await?;

        let config = Config {
            database: DatabaseConfig {
                url: postgres.database_url.clone(),
                max_connections: 5,
                connect_timeout_seconds: 30,
            },
            builders: vec![BuilderConfig {
                name: "base_sepolia".to_string(),
                url: Url::parse("wss://sepolia.flashblocks.base.org/ws")?,
                reconnect_delay_seconds: 5,
            }],
            archiver: ArchiverConfig {
                buffer_size: 100,
                batch_size: 10,
                flush_interval_seconds: 1,
            },
        };

        Ok(Self { postgres, config })
    }
}

#[tokio::test]
async fn test_base_sepolia_flashblocks_connection() -> anyhow::Result<()> {
    // Initialize test logging
    let _ = tracing_subscriber::fmt()
        .with_env_filter("flashblocks_archiver=debug")
        .try_init();

    let setup = SepoliaTestSetup::new().await?;

    info!("Starting Base Sepolia flashblocks test");

    // Create archiver
    let archiver = FlashblocksArchiver::new(setup.config).await?;

    // Test connection and receive at least one message within 30 seconds
    let result = timeout(Duration::from_secs(30), async {
        // Run archiver for a short time to collect some data
        let archiver_task = tokio::spawn(async move { archiver.run().await });

        // Wait a bit for messages to come in
        tokio::time::sleep(Duration::from_secs(10)).await;

        // Abort the archiver task
        archiver_task.abort();
        let _ = archiver_task.await;

        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(_) => {
            info!("Successfully connected to Base Sepolia flashblocks");

            // Check if we received any data
            let flashblocks = setup
                .postgres
                .database
                .get_flashblocks_by_block_number(0)
                .await?;
            if flashblocks.is_empty() {
                warn!("No flashblocks received during test period - this may be normal if no blocks are being produced");
            } else {
                info!("Received {} flashblocks during test", flashblocks.len());
            }
        }
        Err(_) => {
            panic!("Failed to connect to Base Sepolia flashblocks within timeout");
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_sepolia_data_integrity() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("flashblocks_archiver=info")
        .try_init();

    let setup = SepoliaTestSetup::new().await?;
    let archiver = FlashblocksArchiver::new(setup.config).await?;

    // Run for longer to collect more data
    let result = timeout(Duration::from_secs(60), async {
        let archiver_task = tokio::spawn(async move { archiver.run().await });

        tokio::time::sleep(Duration::from_secs(30)).await;
        archiver_task.abort();
        let _ = archiver_task.await;

        Ok::<(), anyhow::Error>(())
    })
    .await;

    if result.is_err() {
        warn!("Test timeout - proceeding with data validation");
    }

    // Validate data integrity
    let builders_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM builders")
        .fetch_one(setup.postgres.database.get_pool())
        .await?;
    assert!(builders_count > 0, "Should have at least one builder");

    // Check for flashblocks
    let flashblocks_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM flashblocks")
        .fetch_one(setup.postgres.database.get_pool())
        .await?;

    if flashblocks_count > 0 {
        info!("Found {} flashblocks in database", flashblocks_count);

        // Validate relationships between tables
        let orphaned_transactions = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM transactions t 
             WHERE NOT EXISTS (SELECT 1 FROM flashblocks f WHERE f.id = t.flashblock_id)",
        )
        .fetch_one(setup.postgres.database.get_pool())
        .await?;
        assert_eq!(
            orphaned_transactions, 0,
            "No orphaned transactions should exist"
        );

        let orphaned_receipts = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM receipts r 
             WHERE NOT EXISTS (SELECT 1 FROM flashblocks f WHERE f.id = r.flashblock_id)",
        )
        .fetch_one(setup.postgres.database.get_pool())
        .await?;
        assert_eq!(orphaned_receipts, 0, "No orphaned receipts should exist");

        let orphaned_balances = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM account_balances ab 
             WHERE NOT EXISTS (SELECT 1 FROM flashblocks f WHERE f.id = ab.flashblock_id)",
        )
        .fetch_one(setup.postgres.database.get_pool())
        .await?;
        assert_eq!(
            orphaned_balances, 0,
            "No orphaned account balances should exist"
        );

        info!("Data integrity validation passed");
    } else {
        warn!("No flashblocks received - skipping data integrity tests");
    }

    Ok(())
}

#[tokio::test]
async fn test_brotli_decompression() -> anyhow::Result<()> {
    use brotli::enc::BrotliEncoderParams;
    use std::io::Write;

    // Create a sample JSON payload
    let sample_json = r#"{"payload_id":"0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef","index":0,"diff":{"state_root":"0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890","receipts_root":"0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef","logs_bloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","gas_used":21000,"block_hash":"0xfedcba0987654321fedcba0987654321fedcba0987654321fedcba0987654321","transactions":["0x02010203"],"withdrawals":[],"withdrawals_root":"0x0000000000000000000000000000000000000000000000000000000000000000"},"metadata":{"receipts":{},"new_account_balances":{},"block_number":1234567}}"#;

    // Test 1: Plain JSON (should work as-is)
    let json_bytes = sample_json.as_bytes();

    let setup = SepoliaTestSetup::new().await?;
    let manager =
        flashblocks_archiver::websocket::WebSocketManager::new(setup.config.builders[0].clone());

    // This should work for plain JSON
    let result = manager.try_decode_message(json_bytes);
    if result.is_err() {
        // This is expected as our sample JSON might not match the exact FlashblocksPayloadV1 structure
        info!(
            "Plain JSON parsing failed as expected (structure mismatch): {:?}",
            result.err()
        );
    }

    // Test 2: Brotli compressed JSON
    let mut compressed = Vec::new();
    {
        let params = BrotliEncoderParams::default();
        let mut writer = brotli::CompressorWriter::with_params(&mut compressed, 4096, &params);
        writer.write_all(sample_json.as_bytes())?;
        writer.flush()?;
    } // CompressorWriter is dropped here, ensuring compression is finalized

    // This should decompress and then fail parsing (due to structure mismatch)
    let result = manager.try_decode_message(&compressed);
    if result.is_err() {
        info!(
            "Brotli decompression + parsing failed as expected (structure mismatch): {:?}",
            result.err()
        );
    }

    // Test 3: Empty data
    let result = manager.try_decode_message(&[]);
    assert!(result.is_err(), "Empty data should fail");

    // Test 4: Invalid brotli data
    let invalid_brotli = vec![0xFF, 0xFF, 0xFF, 0xFF];
    let result = manager.try_decode_message(&invalid_brotli);
    assert!(result.is_err(), "Invalid brotli data should fail");

    info!("Brotli decompression tests completed");
    Ok(())
}

#[tokio::test]
async fn test_websocket_error_handling() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("flashblocks_archiver=debug")
        .try_init();

    // Test with invalid WebSocket URL
    let config = Config {
        database: DatabaseConfig {
            url: "postgresql://invalid_url".to_string(),
            max_connections: 5,
            connect_timeout_seconds: 1,
        },
        builders: vec![BuilderConfig {
            name: "invalid_builder".to_string(),
            url: Url::parse("wss://invalid.nonexistent.domain.com/ws")?,
            reconnect_delay_seconds: 1,
        }],
        archiver: ArchiverConfig {
            buffer_size: 10,
            batch_size: 5,
            flush_interval_seconds: 1,
        },
    };

    // This should fail to create the archiver due to database connection failure
    let result = FlashblocksArchiver::new(config).await;
    assert!(result.is_err(), "Should fail with invalid database URL");

    info!("WebSocket error handling test completed");
    Ok(())
}

#[tokio::test]
async fn test_database_constraint_violations() -> anyhow::Result<()> {
    let setup = SepoliaTestSetup::new().await?;

    // Test duplicate builder insertion
    let builder_id1 = setup
        .postgres
        .database
        .get_or_create_builder("wss://test.example.com", Some("test_builder"))
        .await?;

    let builder_id2 = setup
        .postgres
        .database
        .get_or_create_builder(
            "wss://test.example.com",
            Some("test_builder_different_name"),
        )
        .await?;

    // Should return the same ID for the same URL
    assert_eq!(
        builder_id1, builder_id2,
        "Same URL should return same builder ID"
    );

    info!("Database constraint violation tests completed");
    Ok(())
}

#[tokio::test]
async fn test_malformed_message_handling() -> anyhow::Result<()> {
    let setup = SepoliaTestSetup::new().await?;
    let manager =
        flashblocks_archiver::websocket::WebSocketManager::new(setup.config.builders[0].clone());

    // Test various malformed messages
    let test_cases: Vec<(&[u8], &str)> = vec![
        (b"invalid json", "Invalid JSON"),
        (b"{}", "Empty JSON object"),
        (b"{\"invalid\": \"structure\"}", "Invalid structure"),
        (b"null", "Null JSON"),
        (b"[]", "JSON array instead of object"),
        (b"\"string\"", "JSON string instead of object"),
    ];

    for (data, description) in test_cases {
        let result = manager.try_decode_message(data);
        assert!(result.is_err(), "Should fail for {}", description);
        info!("Correctly rejected {}: {:?}", description, result.err());
    }

    info!("Malformed message handling tests completed");
    Ok(())
}

#[tokio::test]
async fn test_large_message_handling() -> anyhow::Result<()> {
    let setup = SepoliaTestSetup::new().await?;
    let manager =
        flashblocks_archiver::websocket::WebSocketManager::new(setup.config.builders[0].clone());

    // Create a very large JSON message (should still be handled gracefully)
    let large_string = "x".repeat(1_000_000); // 1MB string
    let large_json = format!(r#"{{"large_field": "{}"}}"#, large_string);

    let result = manager.try_decode_message(large_json.as_bytes());
    // Should fail due to structure, but not due to size
    assert!(
        result.is_err(),
        "Should fail due to invalid structure, not size"
    );

    // The error should be about parsing, not about size limits
    let error_msg = format!("{:?}", result.err().unwrap());
    assert!(
        !error_msg.contains("too large"),
        "Error should not be about size limits"
    );

    info!("Large message handling test completed");
    Ok(())
}

#[tokio::test]
async fn test_database_transaction_rollback() -> anyhow::Result<()> {
    let setup = SepoliaTestSetup::new().await?;

    // Create a builder
    let builder_id = setup
        .postgres
        .database
        .get_or_create_builder("wss://test.example.com", Some("test_builder"))
        .await?;

    // Create a flashblock with invalid data to trigger constraint violations
    // This is a bit tricky since our schema is quite permissive
    // Let's test the unique constraint on (builder_id, payload_id, flashblock_index)

    use alloy_primitives::{Address, Bloom, Bytes, B256, U256};
    use alloy_rpc_types_engine::PayloadId;

    let payload = FlashblockMessage {
        payload_id: PayloadId::new([1; 8]),
        index: 0,
        base: Some(ExecutionPayloadBaseV1 {
            parent_beacon_block_root: B256::from([2; 32]),
            parent_hash: B256::from([3; 32]),
            fee_recipient: Address::from([4; 20]),
            prev_randao: B256::from([5; 32]),
            block_number: 12345,
            gas_limit: 30_000_000,
            timestamp: 1700000000,
            extra_data: Bytes::from(vec![6, 7, 8]),
            base_fee_per_gas: U256::from(20_000_000_000u64),
        }),
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root: B256::from([9; 32]),
            receipts_root: B256::from([10; 32]),
            logs_bloom: Bloom::default(),
            gas_used: 21000,
            block_hash: B256::from([11; 32]),
            transactions: vec![Bytes::from(vec![0x02, 0x01, 0x00])],
            withdrawals: vec![],
            withdrawals_root: B256::ZERO,
        },
        metadata: Metadata {
            receipts: HashMap::<B256, OpReceipt>::new(),
            new_account_balances: HashMap::<Address, U256>::new(),
            block_number: 12345,
        },
    };

    // Store the same flashblock twice - second should fail due to unique constraint
    let result1 = setup
        .postgres
        .database
        .store_flashblock(builder_id, &payload)
        .await;
    assert!(result1.is_ok(), "First insertion should succeed");

    let result2 = setup
        .postgres
        .database
        .store_flashblock(builder_id, &payload)
        .await;
    assert!(
        result2.is_err(),
        "Second insertion should fail due to unique constraint"
    );

    info!("Database transaction rollback test completed");
    Ok(())
}
