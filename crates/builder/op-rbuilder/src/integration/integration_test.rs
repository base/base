#[cfg(all(test, feature = "integration"))]
mod tests {
    use crate::{
        integration::{
            op_rbuilder::OpRbuilderConfig, op_reth::OpRethConfig, IntegrationFramework,
            TestHarness, TestHarnessBuilder,
        },
        tester::{BlockGenerator, EngineApi},
        tx_signer::Signer,
    };
    use alloy_consensus::{Transaction, TxEip1559};
    use alloy_eips::{eip1559::MIN_PROTOCOL_BASE_FEE, eip2718::Encodable2718};
    use alloy_primitives::hex;
    use alloy_provider::{Identity, Provider, ProviderBuilder};
    use alloy_rpc_types_eth::BlockTransactionsKind;
    use futures_util::StreamExt;
    use op_alloy_consensus::OpTypedTransaction;
    use op_alloy_network::Optimism;
    use std::{
        cmp::max,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio_tungstenite::connect_async;
    use uuid::Uuid;

    const BUILDER_PRIVATE_KEY: &str =
        "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    #[tokio::test]
    #[cfg(not(feature = "flashblocks"))]
    async fn integration_test_chain_produces_blocks() -> eyre::Result<()> {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework =
            IntegrationFramework::new("integration_test_chain_produces_blocks").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1234)
            .network_port(1235)
            .http_port(1238)
            .with_builder_private_key(BUILDER_PRIVATE_KEY);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1236)
            .network_port(1237);

        framework.start("op-reth", &reth).await.unwrap();

        let op_rbuilder = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        let engine_api = EngineApi::new("http://localhost:1234").unwrap();
        let validation_api = EngineApi::new("http://localhost:1236").unwrap();

        let mut generator = BlockGenerator::new(engine_api, Some(validation_api), false, 1, None);
        generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .on_http("http://localhost:1238".parse()?);

        for _ in 0..10 {
            let block_hash = generator.generate_block().await?;

            // query the block and the transactions inside the block
            let block = provider
                .get_block_by_hash(block_hash)
                .await?
                .expect("block");

            for hash in block.transactions.hashes() {
                let _ = provider
                    .get_transaction_receipt(hash)
                    .await?
                    .expect("receipt");
            }
        }

        Ok(())
    }

    #[tokio::test]
    #[cfg(not(feature = "flashblocks"))]
    async fn integration_test_monitor_transaction_drops() -> eyre::Result<()> {
        // This test ensures that the transactions that get reverted an not included in the block
        // are emitted as a log on the builder.
        let harness = TestHarnessBuilder::new("integration_test_monitor_transaction_drops")
            .with_revert_protection()
            .build()
            .await?;

        let mut generator = harness.block_generator().await?;

        // send 10 reverting transactions
        let mut pending_txn = Vec::new();
        for _ in 0..10 {
            let txn = harness.send_revert_transaction().await?;
            pending_txn.push(txn);
        }

        // generate 10 blocks
        for _ in 0..10 {
            let block_hash = generator.generate_block().await?;

            // query the block and the transactions inside the block
            let block = harness
                .provider()?
                .get_block_by_hash(block_hash)
                .await?
                .expect("block");

            // blocks should only include two transactions (deposit + builder)
            assert_eq!(block.transactions.len(), 2);
        }

        // check that the builder emitted logs for the reverted transactions
        // with the monitoring logic
        // TODO: this is not ideal, lets find a different way to detect this
        // Each time a transaction is dropped, it emits a log like this
        // 'Transaction event received target="monitoring" tx_hash="<tx_hash>" kind="discarded"'
        let builder_logs = std::fs::read_to_string(harness.builder_log_path)?;

        for txn in pending_txn {
            let txn_log = format!(
                "Transaction event received target=\"monitoring\" tx_hash=\"{}\" kind=\"discarded\"",
                txn.tx_hash()
            );

            assert!(builder_logs.contains(txn_log.as_str()));
        }

        Ok(())
    }

    #[tokio::test]
    #[cfg(not(feature = "flashblocks"))]
    async fn integration_test_revert_protection_disabled() -> eyre::Result<()> {
        let harness = TestHarnessBuilder::new("integration_test_revert_protection_disabled")
            .build()
            .await?;

        let mut generator = harness.block_generator().await?;

        let txn1 = harness.send_valid_transaction().await?;
        let txn2 = harness.send_revert_transaction().await?;
        let pending_txn = vec![txn1, txn2];

        let block_hash = generator.generate_block().await?;

        // the transactions should be included in the block now
        let pending_txn = {
            let mut transaction_hashes = Vec::new();
            for txn in pending_txn {
                let txn_hash = txn.with_timeout(None).watch().await?;
                transaction_hashes.push(txn_hash);
            }
            transaction_hashes
        };

        // validate that all the transaction hashes are included in the block
        let provider = harness.provider()?;
        let block = provider
            .get_block_by_hash(block_hash)
            .await?
            .expect("block");

        for txn in pending_txn {
            assert!(block.transactions.hashes().any(|hash| hash == txn));
        }

        Ok(())
    }

    #[tokio::test]
    #[cfg(not(feature = "flashblocks"))]
    async fn integration_test_revert_protection() -> eyre::Result<()> {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework =
            IntegrationFramework::new("integration_test_revert_protection").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1244)
            .network_port(1245)
            .http_port(1248)
            .with_builder_private_key(BUILDER_PRIVATE_KEY)
            .with_revert_protection(true);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1246)
            .network_port(1247);

        framework.start("op-reth", &reth).await.unwrap();

        let _ = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        let engine_api = EngineApi::new("http://localhost:1244").unwrap();
        let validation_api = EngineApi::new("http://localhost:1246").unwrap();

        let mut generator = BlockGenerator::new(engine_api, Some(validation_api), false, 1, None);
        let latest_block = generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .on_http("http://localhost:1248".parse()?);

        let mut base_fee = max(
            latest_block.header.base_fee_per_gas.unwrap(),
            MIN_PROTOCOL_BASE_FEE,
        );
        for _ in 0..10 {
            // Get builder's address
            let known_wallet = Signer::try_from_secret(BUILDER_PRIVATE_KEY.parse()?)?;
            let builder_address = known_wallet.address;
            // Get current nonce from chain
            let nonce = provider.get_transaction_count(builder_address).await?;
            // Transaction from builder should succeed
            let tx_request = OpTypedTransaction::Eip1559(TxEip1559 {
                chain_id: 901,
                nonce,
                gas_limit: 210000,
                max_fee_per_gas: base_fee.into(),
                ..Default::default()
            });
            let signed_tx = known_wallet.sign_tx(tx_request)?;
            let known_tx = provider
                .send_raw_transaction(signed_tx.encoded_2718().as_slice())
                .await?;

            // Create a reverting transaction
            let tx_request = OpTypedTransaction::Eip1559(TxEip1559 {
                chain_id: 901,
                nonce: nonce + 1,
                gas_limit: 300000,
                max_fee_per_gas: base_fee.into(),
                input: hex!("60006000fd").into(), // PUSH1 0x00 PUSH1 0x00 REVERT
                ..Default::default()
            });
            let signed_tx = known_wallet.sign_tx(tx_request)?;
            let reverting_tx = provider
                .send_raw_transaction(signed_tx.encoded_2718().as_slice())
                .await?;

            let block_hash = generator.generate_block().await?;

            // query the block and the transactions inside the block
            let block = provider
                .get_block_by_hash(block_hash)
                .await?
                .expect("block");

            // Verify known transaction is included
            assert!(
                block
                    .transactions
                    .hashes()
                    .any(|hash| hash == *known_tx.tx_hash()),
                "successful transaction missing from block"
            );

            // Verify reverted transaction is NOT included
            assert!(
                !block
                    .transactions
                    .hashes()
                    .any(|hash| hash == *reverting_tx.tx_hash()),
                "reverted transaction unexpectedly included in block"
            );
            for hash in block.transactions.hashes() {
                let receipt = provider
                    .get_transaction_receipt(hash)
                    .await?
                    .expect("receipt");
                let success = receipt.inner.inner.status();
                assert!(success);
            }
            base_fee = max(
                block.header.base_fee_per_gas.unwrap(),
                MIN_PROTOCOL_BASE_FEE,
            );
        }

        Ok(())
    }

    #[tokio::test]
    #[cfg(not(feature = "flashblocks"))]
    async fn integration_test_fee_priority_ordering() -> eyre::Result<()> {
        // This test validates that transactions are ordered by fee priority in blocks
        let mut framework =
            IntegrationFramework::new("integration_test_fee_priority_ordering").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1264)
            .network_port(1265)
            .http_port(1268)
            .with_builder_private_key(BUILDER_PRIVATE_KEY);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1266)
            .network_port(1267);

        framework.start("op-reth", &reth).await.unwrap();

        let _ = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        let engine_api = EngineApi::new("http://localhost:1264").unwrap();
        let validation_api = EngineApi::new("http://localhost:1266").unwrap();

        let mut generator = BlockGenerator::new(engine_api, Some(validation_api), false, 1, None);
        let latest_block = generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .on_http("http://localhost:1268".parse()?);

        let base_fee = max(
            latest_block.header.base_fee_per_gas.unwrap(),
            MIN_PROTOCOL_BASE_FEE,
        );

        // Create transactions with increasing fee values
        let priority_fees: [u128; 5] = [1, 3, 5, 2, 4]; // Deliberately not in order
        let signers = vec![
            Signer::random(),
            Signer::random(),
            Signer::random(),
            Signer::random(),
            Signer::random(),
        ];
        let mut txs = Vec::new();

        // Fund test accounts with deposits
        for signer in &signers {
            generator
                .deposit(signer.address, 1000000000000000000)
                .await?;
        }

        // Send transactions in non-optimal fee order
        for (i, priority_fee) in priority_fees.iter().enumerate() {
            let tx_request = OpTypedTransaction::Eip1559(TxEip1559 {
                chain_id: 901,
                nonce: 1,
                gas_limit: 210000,
                max_fee_per_gas: base_fee as u128 + *priority_fee,
                max_priority_fee_per_gas: *priority_fee,
                ..Default::default()
            });
            let signed_tx = signers[i].sign_tx(tx_request)?;
            let tx = provider
                .send_raw_transaction(signed_tx.encoded_2718().as_slice())
                .await?;
            txs.push(tx);
        }

        // Generate a block that should include these transactions
        let block_hash = generator.generate_block().await?;

        // Query the block and check transaction ordering
        let block = provider
            .get_block_by_hash(block_hash)
            .full()
            .await?
            .expect("block");

        // Verify all transactions are included
        for tx in &txs {
            assert!(
                block
                    .transactions
                    .hashes()
                    .any(|hash| hash == *tx.tx_hash()),
                "transaction missing from block"
            );
        }

        let tx_fees: Vec<_> = block
            .transactions
            .into_transactions()
            .map(|tx| tx.effective_tip_per_gas(base_fee.into()))
            .collect();

        // Verify transactions are ordered by decreasing fee (highest fee first)
        // Skip the first deposit transaction and last builder transaction
        for i in 1..tx_fees.len() - 2 {
            assert!(
                tx_fees[i] >= tx_fees[i + 1],
                "Transactions not ordered by decreasing fee: {:?}",
                tx_fees
            );
        }

        Ok(())
    }

    #[tokio::test]
    #[cfg(not(feature = "flashblocks"))]
    async fn integration_test_get_payload_close_to_fcu() -> eyre::Result<()> {
        let test_harness = TestHarnessBuilder::new("integration_test_get_payload_close_to_fcu")
            .build()
            .await?;
        let mut block_generator = test_harness.block_generator().await?;

        // add some transactions to the pool so that the builder is busy when we send the fcu/getPayload requests
        for _ in 0..10 {
            // Note, for this test it is okay if they are not valid
            let _ = test_harness.send_valid_transaction().await?;
        }

        // TODO: In the fail case scenario, this hangs forever, but it should return an error
        // Figure out how to do timeout (i.e. 1s) on the engine api.
        block_generator.submit_payload(None, 0, true).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg(feature = "flashblocks")]
    async fn integration_test_chain_produces_blocks() -> eyre::Result<()> {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework =
            IntegrationFramework::new("integration_test_chain_produces_blocks").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1234)
            .network_port(1235)
            .http_port(1238)
            .with_builder_private_key(BUILDER_PRIVATE_KEY)
            .with_flashblocks_ws_url("localhost:1239")
            .with_chain_block_time(2000)
            .with_flashbots_block_time(200);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1236)
            .network_port(1237);

        framework.start("op-reth", &reth).await.unwrap();

        let op_rbuilder = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        // Create a struct to hold received messages
        let received_messages = Arc::new(Mutex::new(Vec::new()));
        let messages_clone = received_messages.clone();

        // Spawn WebSocket listener task
        let ws_handle = tokio::spawn(async move {
            let (ws_stream, _) = connect_async("ws://localhost:1239").await?;
            let (_, mut read) = ws_stream.split();

            while let Some(Ok(msg)) = read.next().await {
                if let Ok(text) = msg.into_text() {
                    messages_clone.lock().unwrap().push(text);
                }
            }
            Ok::<_, eyre::Error>(())
        });

        let engine_api = EngineApi::new("http://localhost:1234").unwrap();
        let validation_api = EngineApi::new("http://localhost:1236").unwrap();

        let mut generator = BlockGenerator::new(engine_api, Some(validation_api), false, 2, None);
        generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .on_http("http://localhost:1238".parse()?);

        for _ in 0..10 {
            let block_hash = generator.generate_block().await?;

            // query the block and the transactions inside the block
            let block = provider
                .get_block_by_hash(block_hash)
                .await?
                .expect("block");

            for hash in block.transactions.hashes() {
                let _ = provider
                    .get_transaction_receipt(hash)
                    .await?
                    .expect("receipt");
            }
        }

        // check there's 10 flashblocks log lines (2000ms / 200ms)
        op_rbuilder.find_log_line("Building flashblock 9").await?;

        // Process websocket messages
        let timeout_duration = Duration::from_secs(10);
        tokio::time::timeout(timeout_duration, async {
            let mut message_count = 0;
            loop {
                if message_count >= 10 {
                    break;
                }
                let messages = received_messages.lock().unwrap();
                let messages_json: Vec<serde_json::Value> = messages
                    .iter()
                    .map(|msg| serde_json::from_str(msg).unwrap())
                    .collect();
                for msg in messages_json.iter() {
                    let metadata = msg.get("metadata");
                    assert!(metadata.is_some(), "metadata field missing");
                    let metadata = metadata.unwrap();
                    assert!(
                        metadata.get("block_number").is_some(),
                        "block_number missing"
                    );
                    assert!(
                        metadata.get("new_account_balances").is_some(),
                        "new_account_balances missing"
                    );
                    assert!(metadata.get("receipts").is_some(), "receipts missing");
                    // also check if the length of the receipts is the same as the number of transactions
                    assert!(
                        metadata.get("receipts").unwrap().as_object().unwrap().len()
                            == msg
                                .get("diff")
                                .unwrap()
                                .get("transactions")
                                .unwrap()
                                .as_array()
                                .unwrap()
                                .len(),
                        "receipts length mismatch"
                    );
                    message_count += 1;
                }
                drop(messages);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await?;
        ws_handle.abort();

        Ok(())
    }

    #[tokio::test]
    #[cfg(feature = "flashblocks")]
    async fn integration_test_flashblocks_respects_gas_limit() -> eyre::Result<()> {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework =
            IntegrationFramework::new("integration_test_flashblocks_respects_gas_limit").unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        let block_time_ms = 1000;
        let flashblock_time_ms = 100;

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1244)
            .network_port(1245)
            .http_port(1248)
            .with_builder_private_key(BUILDER_PRIVATE_KEY)
            .with_flashblocks_ws_url("localhost:1249")
            .with_chain_block_time(block_time_ms)
            .with_flashbots_block_time(flashblock_time_ms);

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1246)
            .network_port(1247);

        framework.start("op-reth", &reth).await.unwrap();

        let op_rbuilder = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        let engine_api = EngineApi::new("http://localhost:1244").unwrap();
        let validation_api = EngineApi::new("http://localhost:1246").unwrap();

        let mut generator = BlockGenerator::new(
            engine_api,
            Some(validation_api),
            false,
            block_time_ms / 1000,
            None,
        );
        generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .on_http("http://localhost:1248".parse()?);

        // Delay the payload building by 4s, ensure that the correct number of flashblocks are built
        let block_hash = generator.generate_block_with_delay(4).await?;

        // query the block and the transactions inside the block
        let block = provider
            .get_block_by_hash(block_hash)
            .await?
            .expect("block");

        for hash in block.transactions.hashes() {
            let _ = provider
                .get_transaction_receipt(hash)
                .await?
                .expect("receipt");
        }

        // check there's no more than 10 flashblocks log lines (2000ms / 200ms)
        op_rbuilder.find_log_line("Building flashblock 9").await?;
        op_rbuilder
            .find_log_line("Skipping flashblock reached target=10 idx=10")
            .await?;

        Ok(())
    }
}
