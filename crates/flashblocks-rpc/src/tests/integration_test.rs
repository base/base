#[cfg(test)]
mod tests {
    use crate::flashblocks::Metadata;
    use crate::tests::{op_reth::OpRethConfig, IntegrationFramework};
    use alloy_consensus::Receipt;
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::{address, bytes, Address, Bytes, B256, U256};
    use alloy_provider::Identity;
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_rpc_types_engine::PayloadId;
    use alloy_rpc_types_eth::TransactionInput;
    use futures::SinkExt;
    use futures_util::StreamExt;
    use op_alloy_network::Optimism;
    use op_alloy_network::ReceiptResponse;
    use op_alloy_rpc_types::OpTransactionRequest;
    use reth_optimism_primitives::OpReceipt;
    use rollup_boost::primitives::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
    };
    use serde_json;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;
    use tracing::info;
    use uuid::Uuid;

    fn create_first_payload() -> FlashblocksPayloadV1 {
        FlashblocksPayloadV1 {
            payload_id: PayloadId::new([0; 8]),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::default(),
                parent_hash: B256::default(),
                fee_recipient: Address::ZERO,
                prev_randao: B256::default(),
                block_number: 1,
                gas_limit: 30_000_000,
                timestamp: 0,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::ZERO,
            }),
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: serde_json::to_value(Metadata {
                block_number: 1,
                receipts: HashMap::default(),
                new_account_balances: HashMap::default(),
            })
            .unwrap(),
        }
    }

    fn create_second_payload() -> FlashblocksPayloadV1 {
        // NOTE:
        // To create tx use cast mktx/
        // Example: `cast mktx --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 --nonce 1 --gas-limit 100000 --gas-price 1499576 --chain 84532 --value 0 --priority-gas-price 0 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 0x`
        // Create second payload (index 1) with transactions
        // tx1 hash: 0x2be2e6f8b01b03b87ae9f0ebca8bbd420f174bef0fbcc18c7802c5378b78f548 (deposit transaction)
        // tx2 hash: 0xbb079fbde7d12fd01664483cd810e91014113e405247479e5615974ebca93e4a
        let tx1 = Bytes::from_str("0x7ef8f8a042a8ae5ec231af3d0f90f68543ec8bca1da4f7edd712d5b51b490688355a6db794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e200000044d000a118b00000000000000040000000067cb7cb0000000000077dbd4000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000014edd27304108914dd6503b19b9eeb9956982ef197febbeeed8a9eac3dbaaabdf000000000000000000000000fc56e7272eebbba5bc6c544e159483c4a38f8ba3").unwrap();
        let tx2 = Bytes::from_str("0x02f87383014a3480808449504f80830186a094deaddeaddeaddeaddeaddeaddeaddeaddead00018ad3c21bcb3f6efc39800080c0019f5a6fe2065583f4f3730e82e5725f651cbbaf11dc1f82c8d29ba1f3f99e5383a061e0bf5dfff4a9bc521ad426eee593d3653c5c330ae8a65fad3175d30f291d31").unwrap();
        // Send another test flashblock payload
        let payload = FlashblocksPayloadV1 {
            payload_id: PayloadId::new([0; 8]),
            index: 1,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::default(),
                receipts_root: B256::default(),
                gas_used: 0,
                block_hash: B256::default(),
                transactions: vec![tx1, tx2],
                withdrawals: Vec::new(),
                logs_bloom: Default::default(),
                withdrawals_root: Default::default(),
            },
            metadata: serde_json::to_value(Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        "0x2be2e6f8b01b03b87ae9f0ebca8bbd420f174bef0fbcc18c7802c5378b78f548"
                            .to_string(), // transaction hash as string
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 21000,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        "0xbb079fbde7d12fd01664483cd810e91014113e405247479e5615974ebca93e4a"
                            .to_string(), // transaction hash as string
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 45000,
                            logs: vec![],
                        }),
                    );
                    receipts
                },
                new_account_balances: {
                    let mut map = HashMap::default();
                    map.insert(
                        "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string(),
                        "0x1234".to_string(),
                    );
                    map
                },
            })
            .unwrap(),
        };

        payload
    }

    #[tokio::test]
    async fn integration_test_get_pending_block() -> eyre::Result<()> {
        let mut framework =
            IntegrationFramework::new("integration_test_get_pending_block").unwrap();

        // Start WebSocket server
        let ws_server = tokio::spawn(async move {
            let addr = "127.0.0.1:1239".parse::<SocketAddr>().unwrap();
            let listener = TcpListener::bind(&addr).await.unwrap();
            info!(
                message = "WebSocket server listening",
                address = %addr
            );

            while let Ok((stream, _)) = listener.accept().await {
                let ws_stream = accept_async(stream).await.unwrap();
                let (mut write, mut read) = ws_stream.split();

                // Send test flashblock payload
                let payload = create_first_payload();
                let message = serde_json::to_string(&payload).unwrap();
                write.send(Message::Binary(message.into())).await.unwrap();

                // wait for 5 seconds to send another payload
                tokio::time::sleep(Duration::from_secs(5)).await;

                let payload = create_second_payload();
                let message = serde_json::to_string(&payload).unwrap();
                write.send(Message::Binary(message.into())).await.unwrap();

                // Keep connection alive and handle messages
                while let Some(msg) = read.next().await {
                    if let Ok(_) = msg {
                        // Handle incoming messages if needed
                        continue;
                    }
                    break;
                }
            }
        });

        // Setup genesis path
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("src/tests/assets/genesis.json");
        assert!(genesis_path.exists());

        // Create and start reth-flashblocks node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(reth_data_dir)
            .auth_rpc_port(1236)
            .network_port(1237)
            .http_port(1238)
            .websocket_url("ws://localhost:1239");

        framework.start("base-reth-node", &reth).await.unwrap();

        // Wait for some time to allow messages to be processed
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Create provider to interact with the node
        let provider: alloy_provider::RootProvider<Optimism> =
            ProviderBuilder::<Identity, Identity, Optimism>::default()
                .connect_http("http://localhost:1238".parse()?);

        // Query first subblock
        if let Some(block) = provider
            .get_block_by_number(BlockNumberOrTag::Pending)
            .await?
        {
            // Verify block properties
            assert_eq!(block.header.number, 1);
            assert_eq!(block.transactions.len(), 0);
        } else {
            assert!(false, "no block found");
        }

        // We ensure that eth_call will succeed because we are on plain state
        let eth_call = OpTransactionRequest::default()
            .from(address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"))
            .transaction_type(0)
            .gas_limit(200000)
            .nonce(1)
            .to(address!("0xf39635f2adf40608255779ff742afe13de31f577"))
            .value(U256::from(9999999999849942300000u128))
            .input(TransactionInput::new(bytes!("0x")));
        let res = provider.call(eth_call).await;
        assert!(res.is_ok());

        tokio::time::sleep(Duration::from_secs(3)).await;
        // Query second subblock, now there should be 2 transactions
        if let Some(block) = provider
            .get_block_by_number(BlockNumberOrTag::Pending)
            .await?
        {
            // Verify block properties
            assert_eq!(block.header.number, 1);
            assert_eq!(block.transactions.len(), 2);
        } else {
            assert!(false, "no block found");
        }

        // check transaction receipt
        let receipt = provider
            .get_transaction_receipt(
                B256::from_str(
                    "0x2be2e6f8b01b03b87ae9f0ebca8bbd420f174bef0fbcc18c7802c5378b78f548",
                )
                .unwrap(),
            )
            .await?;
        assert!(receipt.is_some());
        let receipt = receipt.unwrap();
        assert_eq!(receipt.gas_used(), 21000);

        // check transaction receipt
        let receipt = provider
            .get_transaction_receipt(
                B256::from_str(
                    "0xbb079fbde7d12fd01664483cd810e91014113e405247479e5615974ebca93e4a",
                )
                .unwrap(),
            )
            .await?;
        assert!(receipt.is_some());
        assert_eq!(receipt.unwrap().gas_used(), 24000); // 45000 - 21000

        // check transaction by hash
        let tx = provider
            .get_transaction_by_hash(
                B256::from_str(
                    "0x2be2e6f8b01b03b87ae9f0ebca8bbd420f174bef0fbcc18c7802c5378b78f548",
                )
                .unwrap(),
            )
            .await?;
        assert!(tx.is_some());

        let tx = provider
            .get_transaction_by_hash(
                B256::from_str(
                    "0xbb079fbde7d12fd01664483cd810e91014113e405247479e5615974ebca93e4a",
                )
                .unwrap(),
            )
            .await?;
        assert!(tx.is_some());

        // We included heavy spending transaction and now don't have enough funds for this request, so
        // this eth_call with fail
        let eth_call = OpTransactionRequest::default()
            .from(address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"))
            .transaction_type(0)
            .gas_limit(20000000)
            .nonce(1)
            .to(address!("0xf39635f2adf40608255779ff742afe13de31f577"))
            .value(U256::from(9999999999849942300000u128))
            .input(TransactionInput::new(bytes!("0x")));
        let res = provider.call(eth_call).await;
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .as_error_resp()
            .unwrap()
            .message
            .contains("insufficient funds for gas"));

        // check balance
        // use curl command to get balance with pending tag, since alloy provider doesn't support pending tag
        let output = std::process::Command::new("curl")
        .arg("http://localhost:1238")
        .arg("-X")
        .arg("POST")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(r#"{"jsonrpc":"2.0","method":"eth_getBalance","params":["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"],"id":1}"#)
        .output()?;

        let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let balance = U256::from_str(response["result"].as_str().unwrap()).unwrap();
        assert_eq!(balance, U256::from_str("0x1234").unwrap());

        // check nonce
        let output = std::process::Command::new("curl")
        .arg("http://localhost:1238")
        .arg("-X")
        .arg("POST")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(r#"{"jsonrpc":"2.0","method":"eth_getTransactionCount","params":["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"],"id":1}"#)
        .output()?;

        let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let nonce = U256::from_str(response["result"].as_str().unwrap()).unwrap();
        assert_eq!(nonce, U256::from_str("0x1").unwrap());

        // check latest nonce, should still be 0
        let output = std::process::Command::new("curl")
        .arg("http://localhost:1238")
        .arg("-X")
        .arg("POST")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(r#"{"jsonrpc":"2.0","method":"eth_getTransactionCount","params":["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"],"id":1}"#)
        .output()?;

        let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let nonce = U256::from_str(response["result"].as_str().unwrap()).unwrap();
        assert_eq!(nonce, U256::from_str("0x0").unwrap());

        // Don't forget to cleanup
        ws_server.abort();
        Ok(())
    }
}
