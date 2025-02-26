#[cfg(all(test, feature = "integration"))]
mod tests {
    use crate::integration::{
        op_rbuilder::OpRbuilderConfig, op_reth::OpRethConfig, IntegrationFramework,
    };
    use crate::tester::{BlockGenerator, EngineApi};
    use crate::tx_signer::Signer;
    use alloy_consensus::TxEip1559;
    use alloy_eips::eip1559::MIN_PROTOCOL_BASE_FEE;
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::hex;
    use alloy_provider::Identity;
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_rpc_types_eth::BlockTransactionsKind;
    use op_alloy_consensus::OpTypedTransaction;
    use op_alloy_network::Optimism;
    use std::cmp::max;
    use std::path::PathBuf;
    use uuid::Uuid;

    const BUILDER_PRIVATE_KEY: &str =
        "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    #[tokio::test]
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

        let mut generator = BlockGenerator::new(&engine_api, Some(&validation_api), false, 1);
        generator.init().await?;

        let provider = ProviderBuilder::<Identity, Identity, Optimism>::default()
            .on_http("http://localhost:1238".parse()?);

        for _ in 0..10 {
            let block_hash = generator.generate_block().await?;

            // query the block and the transactions inside the block
            let block = provider
                .get_block_by_hash(block_hash, BlockTransactionsKind::Hashes)
                .await?
                .expect("block");

            for hash in block.transactions.hashes() {
                let _ = provider
                    .get_transaction_receipt(hash)
                    .await?
                    .expect("receipt");
            }
        }

        // there must be a line logging the monitoring transaction
        op_rbuilder
            .find_log_line("Committed block built by builder")
            .await?;

        Ok(())
    }

    #[tokio::test]
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
            .with_builder_private_key(BUILDER_PRIVATE_KEY);

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

        let mut generator = BlockGenerator::new(&engine_api, Some(&validation_api), false, 1);
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
                .get_block_by_hash(block_hash, BlockTransactionsKind::Hashes)
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
}
