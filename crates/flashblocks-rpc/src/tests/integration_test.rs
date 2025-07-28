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
        let deposit_tx = Bytes::from_str("0x7ef8f8a042a8ae5ec231af3d0f90f68543ec8bca1da4f7edd712d5b51b490688355a6db794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e200000044d000a118b00000000000000040000000067cb7cb0000000000077dbd4000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000014edd27304108914dd6503b19b9eeb9956982ef197febbeeed8a9eac3dbaaabdf000000000000000000000000fc56e7272eebbba5bc6c544e159483c4a38f8ba3").unwrap();
        let transfer_eth_tx = Bytes::from_str("0x02f87383014a3480808449504f80830186a094deaddeaddeaddeaddeaddeaddeaddeaddead00018ad3c21bcb3f6efc39800080c0019f5a6fe2065583f4f3730e82e5725f651cbbaf11dc1f82c8d29ba1f3f99e5383a061e0bf5dfff4a9bc521ad426eee593d3653c5c330ae8a65fad3175d30f291d31").unwrap();

        let deposit_tx_hash = "0x2be2e6f8b01b03b87ae9f0ebca8bbd420f174bef0fbcc18c7802c5378b78f548";
        let transfer_eth_tx_hash =
            "0xbb079fbde7d12fd01664483cd810e91014113e405247479e5615974ebca93e4a";
        let deployment_tx_hash =
            "0xa9353897b4ab350ae717eefdad4c9cb613e684f5a490c82a44387d8d5a2f8197";
        let increment_tx_hash =
            "0x993ad6a332752f6748636ce899b3791e4a33f7eece82c0db4556c7339c1b2929";

        // NOTE:
        // Following txns deploy a simple Counter contract (Compiled with solc 0.8.13)
        // Only contains a `uin256 public number` and a function increment() { number++ };
        // Following txn calls increment once, so number should be 1
        // Raw Bytecode: 0x608060405234801561001057600080fd5b50610163806100206000396000f3fe608060405234801561001057600080fd5b50600436106100365760003560e01c80638381f58a1461003b578063d09de08a14610059575b600080fd5b610043610063565b604051610050919061009b565b60405180910390f35b610061610069565b005b60005481565b60008081548092919061007b906100e5565b9190505550565b6000819050919050565b61009581610082565b82525050565b60006020820190506100b0600083018461008c565b92915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b60006100f082610082565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff8203610122576101216100b6565b5b60018201905091905056fea2646970667358221220a0719cefc3439563ff433fc58f8ffb66e1b639119206276d3bdac5d2e2b6f2fa64736f6c634300080d0033
        let deployment_tx = Bytes::from_str("0x02f901db83014a3401808449504f8083030d408080b90183608060405234801561001057600080fd5b50610163806100206000396000f3fe608060405234801561001057600080fd5b50600436106100365760003560e01c80638381f58a1461003b578063d09de08a14610059575b600080fd5b610043610063565b604051610050919061009b565b60405180910390f35b610061610069565b005b60005481565b60008081548092919061007b906100e5565b9190505550565b6000819050919050565b61009581610082565b82525050565b60006020820190506100b0600083018461008c565b92915050565b7f4e487b7100000000000000000000000000000000000000000000000000000000600052601160045260246000fd5b60006100f082610082565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff8203610122576101216100b6565b5b60018201905091905056fea2646970667358221220a0719cefc3439563ff433fc58f8ffb66e1b639119206276d3bdac5d2e2b6f2fa64736f6c634300080d0033c080a034278436b367f7b73ab6dc7c7cc09f8880104513f8b8fb691b498257de97a5bca05cb702ebad2aadf9f225bf5f8685ea03d194bf7a2ea05b1d27a1bd33169f9fe0").unwrap();
        // Increment tx: call increment()
        let increment_tx = Bytes::from_str("0x02f86d83014a3402808449504f8082abe094e7f1725e7734ce288f8367e1bb143e90bb3f05128084d09de08ac080a0a9c1a565668084d4052bbd9bc3abce8555a06aed6651c82c2756ac8a83a79fa2a03427f440ce4910a5227ea0cedb60b06cf0bea2dbbac93bd37efa91a474c29d89").unwrap();
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
                transactions: vec![deposit_tx, transfer_eth_tx, deployment_tx, increment_tx],
                withdrawals: Vec::new(),
                logs_bloom: Default::default(),
                withdrawals_root: Default::default(),
            },
            metadata: serde_json::to_value(Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        deposit_tx_hash.to_string(), // transaction hash as string
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 21000,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        transfer_eth_tx_hash.to_string(), // transaction hash as string
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 45000,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        deployment_tx_hash.to_string(), // transaction hash as string
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 172279,
                            logs: vec![],
                        }),
                    );
                    receipts.insert(
                        increment_tx_hash.to_string(), // transaction hash as string
                        OpReceipt::Legacy(Receipt {
                            status: true.into(),
                            cumulative_gas_used: 172279 + 44000,
                            logs: vec![],
                        }),
                    );

                    receipts
                },
                new_account_balances: {
                    let mut map = HashMap::default();
                    map.insert(
                        "0x1234567890123456789012345678901234567890".to_string(),
                        "0x1234".to_string(),
                    );
                    map.insert(
                        // deployed contract address
                        "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512".to_string(),
                        "0x0".to_string(),
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
        // Query second subblock, now there should be 4 transactions
        if let Some(block) = provider
            .get_block_by_number(BlockNumberOrTag::Pending)
            .await?
        {
            // Verify block properties
            assert_eq!(block.header.number, 1);
            assert_eq!(block.transactions.len(), 4);
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
            .nonce(4)
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
        .arg(r#"{"jsonrpc":"2.0","method":"eth_getBalance","params":["0x1234567890123456789012345678901234567890","pending"],"id":1}"#)
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
        assert_eq!(nonce, U256::from_str("0x3").unwrap());

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

        // Counter contract checks

        // check transaction receipt
        let receipt = provider
            .get_transaction_receipt(
                B256::from_str(
                    "0xa9353897b4ab350ae717eefdad4c9cb613e684f5a490c82a44387d8d5a2f8197",
                )
                .unwrap(),
            )
            .await?;
        assert!(receipt.is_some());
        let receipt = receipt.unwrap();
        assert_eq!(receipt.gas_used(), 127279);

        // check transaction receipt
        let receipt = provider
            .get_transaction_receipt(
                B256::from_str(
                    "0x993ad6a332752f6748636ce899b3791e4a33f7eece82c0db4556c7339c1b2929",
                )
                .unwrap(),
            )
            .await?;
        assert!(receipt.is_some());
        assert_eq!(receipt.unwrap().gas_used(), 44000);

        // check transaction by hash
        let tx = provider
            .get_transaction_by_hash(
                B256::from_str(
                    "0xa9353897b4ab350ae717eefdad4c9cb613e684f5a490c82a44387d8d5a2f8197",
                )
                .unwrap(),
            )
            .await?;
        assert!(tx.is_some());

        let tx = provider
            .get_transaction_by_hash(
                B256::from_str(
                    "0x993ad6a332752f6748636ce899b3791e4a33f7eece82c0db4556c7339c1b2929",
                )
                .unwrap(),
            )
            .await?;
        assert!(tx.is_some());

        // read number from counter contract
        let eth_call = OpTransactionRequest::default()
            .from(address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"))
            .transaction_type(0)
            .gas_limit(20000000)
            .nonce(4)
            .to(address!("0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"))
            .value(U256::ZERO)
            .input(TransactionInput::new(bytes!("0x8381f58a")));
        let res = provider.call(eth_call).await;
        assert!(res.is_ok());
        assert_eq!(
            U256::from_str(res.unwrap().to_string().as_str()).unwrap(),
            U256::from(1)
        );

        // Don't forget to cleanup
        ws_server.abort();
        Ok(())
    }
}
