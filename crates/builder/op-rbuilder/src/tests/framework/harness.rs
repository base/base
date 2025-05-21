use super::{
    apis::EngineApi,
    blocks::BlockGenerator,
    op::{OpRbuilderConfig, OpRethConfig},
    service::{self, Service, ServiceInstance},
    TransactionBuilder, BUILDER_PRIVATE_KEY,
};
use alloy_eips::BlockNumberOrTag;
use alloy_network::Network;
use alloy_primitives::hex;
use alloy_provider::{
    Identity, PendingTransactionBuilder, Provider, ProviderBuilder, RootProvider,
};
use op_alloy_network::Optimism;
use parking_lot::Mutex;
use std::{
    collections::HashSet, net::TcpListener, path::PathBuf, sync::LazyLock, time::SystemTime,
};
use time::{format_description, OffsetDateTime};
use uuid::Uuid;

pub struct TestHarnessBuilder {
    name: String,
    use_revert_protection: bool,
    flashblocks_ws_url: Option<String>,
    chain_block_time: Option<u64>,
    flashbots_block_time: Option<u64>,
}

impl TestHarnessBuilder {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            use_revert_protection: false,
            flashblocks_ws_url: None,
            chain_block_time: None,
            flashbots_block_time: None,
        }
    }

    pub fn with_revert_protection(mut self) -> Self {
        self.use_revert_protection = true;
        self
    }

    pub fn with_flashblocks_ws_url(mut self, url: &str) -> Self {
        self.flashblocks_ws_url = Some(url.to_string());
        self
    }

    pub fn with_chain_block_time(mut self, block_time: u64) -> Self {
        self.chain_block_time = Some(block_time);
        self
    }

    pub fn with_flashbots_block_time(mut self, block_time: u64) -> Self {
        self.flashbots_block_time = Some(block_time);
        self
    }

    pub async fn build(self) -> eyre::Result<TestHarness> {
        let mut framework = IntegrationFramework::new(&self.name).unwrap();

        // we are going to use the fixture genesis and copy it to each test folder
        let genesis = include_str!("artifacts/genesis.json.tmpl");

        let mut genesis_path = framework.test_dir.clone();
        genesis_path.push("genesis.json");
        std::fs::write(&genesis_path, genesis)?;

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let builder_auth_rpc_port = get_available_port();
        let builder_http_port = get_available_port();
        let mut op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(builder_auth_rpc_port)
            .network_port(get_available_port())
            .http_port(builder_http_port)
            .with_builder_private_key(BUILDER_PRIVATE_KEY)
            .with_revert_protection(self.use_revert_protection);

        if let Some(flashblocks_ws_url) = self.flashblocks_ws_url {
            op_rbuilder_config = op_rbuilder_config.with_flashblocks_ws_url(&flashblocks_ws_url);
        }

        if let Some(chain_block_time) = self.chain_block_time {
            op_rbuilder_config = op_rbuilder_config.with_chain_block_time(chain_block_time);
        }

        if let Some(flashbots_block_time) = self.flashbots_block_time {
            op_rbuilder_config = op_rbuilder_config.with_flashbots_block_time(flashbots_block_time);
        }

        // create the validation reth node

        let reth_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let validator_auth_rpc_port = get_available_port();
        let reth = OpRethConfig::new()
            .chain_config_path(genesis_path)
            .data_dir(reth_data_dir)
            .auth_rpc_port(validator_auth_rpc_port)
            .network_port(get_available_port());

        framework.start("op-reth", &reth).await.unwrap();

        let builder = framework
            .start("op-rbuilder", &op_rbuilder_config)
            .await
            .unwrap();

        let builder_log_path = builder.log_path.clone();

        Ok(TestHarness {
            _framework: framework,
            builder_auth_rpc_port,
            builder_http_port,
            validator_auth_rpc_port,
            builder_log_path,
        })
    }
}

pub struct TestHarness {
    _framework: IntegrationFramework,
    builder_auth_rpc_port: u16,
    builder_http_port: u16,
    validator_auth_rpc_port: u16,
    builder_log_path: PathBuf,
}

impl TestHarness {
    pub async fn send_valid_transaction(
        &self,
    ) -> eyre::Result<PendingTransactionBuilder<Optimism>> {
        self.create_transaction().send().await
    }

    pub async fn send_revert_transaction(
        &self,
    ) -> eyre::Result<PendingTransactionBuilder<Optimism>> {
        self.create_transaction()
            .with_input(hex!("60006000fd").into()) // PUSH1 0x00 PUSH1 0x00 REVERT
            .send()
            .await
    }

    pub fn provider(&self) -> eyre::Result<RootProvider<Optimism>> {
        let url = format!("http://localhost:{}", self.builder_http_port);
        let provider =
            ProviderBuilder::<Identity, Identity, Optimism>::default().connect_http(url.parse()?);

        Ok(provider)
    }

    pub async fn block_generator(&self) -> eyre::Result<BlockGenerator> {
        let engine_api = EngineApi::new_with_port(self.builder_auth_rpc_port).unwrap();
        let validation_api = Some(EngineApi::new_with_port(self.validator_auth_rpc_port).unwrap());

        let mut generator = BlockGenerator::new(engine_api, validation_api, false, 1, None);
        generator.init().await?;

        Ok(generator)
    }

    pub fn create_transaction(&self) -> TransactionBuilder {
        TransactionBuilder::new(self.provider().expect("provider not available"))
    }

    pub async fn latest_block(&self) -> <Optimism as Network>::BlockResponse {
        self.provider()
            .expect("provider not available")
            .get_block_by_number(BlockNumberOrTag::Latest)
            .full()
            .await
            .expect("failed to get latest block by hash")
            .expect("latest block should exist")
    }

    pub async fn latest_base_fee(&self) -> u128 {
        self.latest_block()
            .await
            .header
            .base_fee_per_gas
            .expect("Base fee per gas not found in the latest block header") as u128
    }

    pub const fn builder_private_key() -> &'static str {
        BUILDER_PRIVATE_KEY
    }

    pub const fn builder_log_path(&self) -> &PathBuf {
        &self.builder_log_path
    }
}

pub fn get_available_port() -> u16 {
    static CLAIMED_PORTS: LazyLock<Mutex<HashSet<u16>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    loop {
        let port: u16 = rand::random_range(1000..20000);
        if TcpListener::bind(("127.0.0.1", port)).is_ok() && CLAIMED_PORTS.lock().insert(port) {
            return port;
        }
    }
}

#[derive(Debug)]
pub enum IntegrationError {
    SpawnError,
    BinaryNotFound,
    SetupError,
    LogError,
    ServiceAlreadyRunning,
}

struct IntegrationFramework {
    test_dir: PathBuf,
    services: Vec<ServiceInstance>,
}

impl IntegrationFramework {
    pub fn new(test_name: &str) -> Result<Self, IntegrationError> {
        let dt: OffsetDateTime = SystemTime::now().into();
        let format = format_description::parse("[year]_[month]_[day]_[hour]_[minute]_[second]")
            .map_err(|_| IntegrationError::SetupError)?;

        let date_format = dt
            .format(&format)
            .map_err(|_| IntegrationError::SetupError)?;

        let mut test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        test_dir.push("../../integration_logs");
        test_dir.push(format!("{date_format}_{test_name}"));

        std::fs::create_dir_all(&test_dir).map_err(|_| IntegrationError::SetupError)?;

        Ok(Self {
            test_dir,
            services: Vec::new(),
        })
    }

    pub async fn start<T: Service>(
        &mut self,
        name: &str,
        config: &T,
    ) -> Result<&mut ServiceInstance, service::Error> {
        let service = self.create_service(name)?;
        service.start_with_config(config).await?;
        Ok(service)
    }

    pub fn create_service(&mut self, name: &str) -> Result<&mut ServiceInstance, service::Error> {
        let service = ServiceInstance::new(name.to_string(), self.test_dir.clone());
        self.services.push(service);
        Ok(self.services.last_mut().unwrap())
    }
}
