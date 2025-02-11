#[cfg(all(test, feature = "integration"))]
mod tests {
    use crate::integration::{
        op_rbuilder::OpRbuilderConfig, op_reth::OpRethConfig, IntegrationFramework,
    };
    use crate::tester::{BlockGenerator, EngineApi};
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_rpc_types_eth::BlockTransactionsKind;
    use op_alloy_network::Optimism;
    use std::path::PathBuf;
    use uuid::Uuid;

    const BUILDER_PRIVATE_KEY: &str =
        "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    #[tokio::test]
    async fn integration_test_chain_produces_blocks() -> eyre::Result<()> {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework = IntegrationFramework::new().unwrap();

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

        let provider = ProviderBuilder::new()
            .network::<Optimism>()
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
}
