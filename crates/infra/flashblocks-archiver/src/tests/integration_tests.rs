use crate::{
    cli::BuilderConfig, tests::common::PostgresTestContainer, types::Metadata,
    websocket::WebSocketManager, Database, FlashblockMessage,
};
use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    utils::parse_ether,
    Address, Bloom, Bytes, B256, U256,
};
use alloy_rpc_types::Withdrawal;
use alloy_rpc_types_engine::PayloadId;
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
use tracing::info;
use url::Url;
use uuid::Uuid;

struct TestSetup {
    postgres: PostgresTestContainer,
    builder_id: Uuid,
}

impl TestSetup {
    async fn new() -> anyhow::Result<Self> {
        let postgres = PostgresTestContainer::new("test_flashblocks").await?;
        let builder_id = postgres
            .create_test_builder("ws://test-builder.example.com", Some("test_builder"))
            .await?;

        Ok(Self {
            postgres,
            builder_id,
        })
    }

    fn database(&self) -> &Database {
        &self.postgres.database
    }

    fn create_test_flashblock(&self, block_number: u64, index: u64) -> FlashblockMessage {
        use alloy_consensus::Eip658Value;
        use alloy_consensus::Receipt;

        let mut receipts = HashMap::<B256, OpReceipt>::new();
        receipts.insert(
            B256::from([1; 32]),
            OpReceipt::Eip1559(Receipt {
                status: Eip658Value::success(),
                cumulative_gas_used: 21000,
                logs: vec![],
            }),
        );

        let mut new_account_balances = HashMap::<Address, U256>::new();
        new_account_balances.insert(
            Address::from([1; 20]),
            parse_ether("2").unwrap(), // 2 ETH in wei
        );

        FlashblockMessage {
            payload_id: PayloadId::new([((block_number + index) % 256) as u8; 8]),
            index,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::from([3; 32]),
                parent_hash: B256::from([4; 32]),
                fee_recipient: Address::from([5; 20]),
                prev_randao: B256::from([6; 32]),
                block_number,
                gas_limit: 30_000_000,
                timestamp: 1700000000,
                extra_data: Bytes::from(vec![7, 8, 9]),
                base_fee_per_gas: U256::from(20_000_000_000u64),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::from([10; 32]),
                receipts_root: B256::from([11; 32]),
                logs_bloom: Bloom::default(),
                gas_used: 21000,
                block_hash: B256::from([12; 32]),
                transactions: vec![Bytes::from(vec![0x02, 0x01, 0x00])],
                withdrawals: vec![Withdrawal {
                    index: 1,
                    validator_index: 100,
                    address: Address::from([13; 20]),
                    amount: 1000000,
                }],
                withdrawals_root: B256::from([14; 32]),
            },
            metadata: Metadata {
                receipts,
                new_account_balances,
                block_number,
            },
        }
    }
}

#[tokio::test]
async fn test_store_and_retrieve_flashblock() -> anyhow::Result<()> {
    let setup = TestSetup::new().await?;
    let flashblock = setup.create_test_flashblock(12345, 0);

    // Store the flashblock
    let flashblock_id = setup
        .database()
        .store_flashblock(setup.builder_id, &flashblock)
        .await?;
    assert!(!flashblock_id.is_nil());

    // Retrieve flashblocks by block number
    let stored_flashblocks = setup
        .database()
        .get_flashblocks_by_block_number(12345)
        .await?;
    assert_eq!(stored_flashblocks.len(), 1);

    let stored = &stored_flashblocks[0];
    assert_eq!(stored.block_number, 12345);
    assert_eq!(stored.flashblock_index, 0);
    assert_eq!(stored.builder_id, setup.builder_id);
    assert_eq!(stored.payload_id, "0x3939393939393939");

    Ok(())
}

#[tokio::test]
async fn test_multiple_flashblocks_same_block() -> anyhow::Result<()> {
    let setup = TestSetup::new().await?;

    // Store multiple flashblocks for the same block
    let flashblock1 = setup.create_test_flashblock(12346, 0);
    let flashblock2 = setup.create_test_flashblock(12346, 1);

    setup
        .database()
        .store_flashblock(setup.builder_id, &flashblock1)
        .await?;
    setup
        .database()
        .store_flashblock(setup.builder_id, &flashblock2)
        .await?;

    // Retrieve all flashblocks for the block
    let stored_flashblocks = setup
        .database()
        .get_flashblocks_by_block_number(12346)
        .await?;
    assert_eq!(stored_flashblocks.len(), 2);

    // Should be ordered by flashblock_index
    assert_eq!(stored_flashblocks[0].flashblock_index, 0);
    assert_eq!(stored_flashblocks[1].flashblock_index, 1);

    Ok(())
}

#[tokio::test]
async fn test_get_latest_block_number() -> anyhow::Result<()> {
    let setup = TestSetup::new().await?;

    // Initially no blocks
    let latest = setup
        .database()
        .get_latest_block_number(setup.builder_id)
        .await?;
    assert_eq!(latest, None);

    // Store some flashblocks
    let flashblock1 = setup.create_test_flashblock(100, 0);
    let flashblock2 = setup.create_test_flashblock(101, 0);
    let flashblock3 = setup.create_test_flashblock(99, 0);

    setup
        .database()
        .store_flashblock(setup.builder_id, &flashblock1)
        .await?;
    setup
        .database()
        .store_flashblock(setup.builder_id, &flashblock2)
        .await?;
    setup
        .database()
        .store_flashblock(setup.builder_id, &flashblock3)
        .await?;

    // Should return the highest block number
    let latest = setup
        .database()
        .get_latest_block_number(setup.builder_id)
        .await?;
    assert_eq!(latest, Some(101));

    Ok(())
}

#[tokio::test]
async fn test_builder_management() -> anyhow::Result<()> {
    let setup = TestSetup::new().await?;

    // Create a new builder
    let builder_id1 = setup
        .database()
        .get_or_create_builder("ws://builder1.example.com", Some("Builder 1"))
        .await?;

    // Creating the same builder again should return the same ID
    let builder_id2 = setup
        .database()
        .get_or_create_builder("ws://builder1.example.com", Some("Builder 1"))
        .await?;

    assert_eq!(builder_id1, builder_id2);

    // Different URL should create different builder
    let builder_id3 = setup
        .database()
        .get_or_create_builder("ws://builder2.example.com", Some("Builder 2"))
        .await?;

    assert_ne!(builder_id1, builder_id3);

    Ok(())
}

#[tokio::test]
async fn test_flashblock_with_no_base() -> anyhow::Result<()> {
    let setup = TestSetup::new().await?;

    let mut flashblock = setup.create_test_flashblock(12347, 0);
    flashblock.base = None; // No base payload

    let flashblock_id = setup
        .database()
        .store_flashblock(setup.builder_id, &flashblock)
        .await?;
    assert!(!flashblock_id.is_nil());

    let stored_flashblocks = setup
        .database()
        .get_flashblocks_by_block_number(12347)
        .await?;
    assert_eq!(stored_flashblocks.len(), 1);

    let stored = &stored_flashblocks[0];
    // Base fields should be None
    assert!(stored.parent_beacon_block_root.is_none());
    assert!(stored.parent_hash.is_none());
    assert!(stored.fee_recipient.is_none());

    Ok(())
}

#[tokio::test]
async fn test_malformed_brotli_edge_cases() -> anyhow::Result<()> {
    let manager = WebSocketManager::new(BuilderConfig {
        name: "test_manager".to_string(),
        url: Url::parse("wss://test.example.com")?,
        reconnect_delay_seconds: 5,
    });

    info!(message = "Testing malformed brotli edge cases");

    let test_cases = vec![
        // Empty data
        (vec![], "empty data"),
        // Single byte that's not '{'
        (vec![0xFF], "single invalid byte"),
        // Looks like JSON start but isn't valid UTF-8
        (vec![b'{', 0xFF, 0xFE], "invalid UTF-8 after {"),
        // Valid brotli header but corrupted data
        (vec![0x1B, 0x00, 0x00], "corrupted brotli"),
        // Very small "JSON" that's actually invalid
        (b"{".to_vec(), "incomplete JSON"),
    ];

    for (data, description) in test_cases {
        let result = manager.try_decode_message(&data);
        assert!(result.is_err(), "Should fail for: {}", description);
        info!(message = "Correctly rejected test case", description = %description, error = ?result.err());
    }

    // Test brotli compression/decompression round trip with invalid JSON
    use brotli::enc::BrotliEncoderParams;
    use std::io::Write;

    let invalid_json = "{invalid json structure";
    let mut compressed = Vec::new();
    {
        let params = BrotliEncoderParams::default();
        let mut writer = brotli::CompressorWriter::with_params(&mut compressed, 4096, &params);
        writer.write_all(invalid_json.as_bytes())?;
        writer.flush()?;
    }

    let result = manager.try_decode_message(&compressed);
    assert!(
        result.is_err(),
        "Should fail to parse decompressed invalid JSON"
    );

    info!(message = "Malformed brotli edge cases test completed");
    Ok(())
}

#[tokio::test]
async fn test_websocket_manager_edge_cases() -> anyhow::Result<()> {
    let manager = WebSocketManager::new(BuilderConfig {
        name: "edge_case_test".to_string(),
        url: Url::parse("wss://test.example.com")?,
        reconnect_delay_seconds: 1,
    });

    info!(message = "Testing WebSocket manager edge cases");

    // Test with various edge case inputs
    let edge_cases = vec![
        // Maximum size JSON-like string (but invalid structure)
        (
            format!("{{{}}}", "x".repeat(100_000)).into_bytes(), // Reduced size for test speed
            "large JSON object",
        ),
        // JSON that starts correctly but becomes invalid
        (
            b"{\"valid_start\": \xFF\xFF".to_vec(),
            "JSON with invalid bytes",
        ),
        // Nested JSON structure
        (
            b"{\"a\":{\"b\":{\"c\":{\"d\":{}}}}}".to_vec(),
            "deeply nested JSON",
        ),
        // JSON with unicode characters
        (
            "{\"unicode\": \"🔥💎🚀\"}".as_bytes().to_vec(),
            "unicode JSON",
        ),
        // JSON with escape sequences
        (
            b"{\"escaped\": \"\\\"quoted\\\" and \\\\backslash\\\\\"}".to_vec(),
            "escaped JSON",
        ),
    ];

    for (data, description) in edge_cases {
        let result = manager.try_decode_message(&data);
        // All should fail due to structure mismatch, but not due to parsing errors
        assert!(result.is_err(), "Should fail for: {}", description);

        let error_msg = format!("{:?}", result.err().unwrap());
        info!(message = "Edge case failed", description = %description, error = %error_msg);
    }

    info!(message = "WebSocket manager edge cases test completed");
    Ok(())
}

#[tokio::test]
async fn test_database_transaction_isolation() -> anyhow::Result<()> {
    let setup = TestSetup::new().await?;

    info!(message = "Testing database transaction isolation");

    // Create two identical flashblocks to test unique constraint handling
    let flashblock = setup.create_test_flashblock(88888, 0);

    // Try to insert the same flashblock from two concurrent tasks
    let database1 = Database::new(&setup.postgres.database_url, 5).await?;
    let database2 = Database::new(&setup.postgres.database_url, 5).await?;

    let flashblock_clone = flashblock.clone();
    let handle1 = tokio::spawn(async move {
        database1
            .store_flashblock(setup.builder_id, &flashblock)
            .await
    });

    let handle2 = tokio::spawn(async move {
        database2
            .store_flashblock(setup.builder_id, &flashblock_clone)
            .await
    });

    let (result1, result2) = tokio::join!(handle1, handle2);

    // With UPSERT, both should succeed (second one updates the first)
    let success_count = [result1?, result2?].iter().filter(|r| r.is_ok()).count();

    assert_eq!(
        success_count, 2,
        "Both insertions should succeed with UPSERT behavior"
    );

    // Verify only one flashblock exists
    let count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM flashblocks WHERE block_number = 88888")
            .fetch_one(setup.database().get_pool())
            .await?;

    assert_eq!(count, 1, "Only one flashblock should exist in database");

    info!(message = "Database transaction isolation test completed");
    Ok(())
}
