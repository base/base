#[cfg(all(test, feature = "integration"))]
mod tests {
    use crate::integration::{
        op_rbuilder::OpRbuilderConfig, op_reth::OpRethConfig, IntegrationFramework,
    };
    use crate::tester::{BlockGenerator, EngineApi};
    use std::path::PathBuf;
    use uuid::Uuid;

    #[tokio::test]
    async fn integration_test_chain_produces_blocks() {
        // This is a simple test using the integration framework to test that the chain
        // produces blocks.
        let mut framework = IntegrationFramework::new().unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let builder = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(1234)
            .network_port(1235);

        framework.start("op-rbuilder", &builder).await.unwrap();

        // create the validation reth node
        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(1236)
            .network_port(1237);

        framework.start("op-reth", &reth).await.unwrap();

        let engine_api = EngineApi::new("http://localhost:1234").unwrap();
        let validation_api = EngineApi::new("http://localhost:1236").unwrap();

        let mut generator = BlockGenerator::new(&engine_api, Some(&validation_api), false, 1);
        generator.init().await.unwrap();

        for _ in 0..10 {
            generator.generate_block().await.unwrap();
        }
    }
}
