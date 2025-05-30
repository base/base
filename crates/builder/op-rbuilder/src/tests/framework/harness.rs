use super::{
    apis::EngineApi,
    blocks::BlockGenerator,
    op::{OpRbuilderConfig, OpRethConfig},
    service::{self, Service, ServiceInstance},
    TransactionBuilder, BUILDER_PRIVATE_KEY,
};
use alloy_eips::BlockNumberOrTag;
use alloy_network::Network;
use alloy_primitives::{hex, B256};
use alloy_provider::{
    ext::TxPoolApi, Identity, PendingTransactionBuilder, Provider, ProviderBuilder, RootProvider,
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
    flashblocks_port: Option<u16>,
    chain_block_time: Option<u64>,
    flashbots_block_time: Option<u64>,
    namespaces: Option<String>,
    extra_params: Option<String>,
}

impl TestHarnessBuilder {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            use_revert_protection: false,
            flashblocks_port: None,
            chain_block_time: None,
            flashbots_block_time: None,
            namespaces: None,
            extra_params: None,
        }
    }

    pub fn with_revert_protection(mut self) -> Self {
        self.use_revert_protection = true;
        self
    }

    pub fn with_flashblocks_port(mut self, port: u16) -> Self {
        self.flashblocks_port = Some(port);
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

    pub fn with_namespaces(mut self, namespaces: &str) -> Self {
        self.namespaces = Some(namespaces.to_string());
        self
    }

    pub fn with_extra_params(mut self, extra_params: &str) -> Self {
        self.extra_params = Some(extra_params.to_string());
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
        let builder_data_dir: PathBuf = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let builder_auth_rpc_port = get_available_port();
        let builder_http_port = get_available_port();
        let mut op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(builder_auth_rpc_port)
            .network_port(get_available_port())
            .http_port(builder_http_port)
            .with_builder_private_key(BUILDER_PRIVATE_KEY)
            .with_revert_protection(self.use_revert_protection)
            .with_namespaces(self.namespaces)
            .with_extra_params(self.extra_params);
        if let Some(flashblocks_port) = self.flashblocks_port {
            op_rbuilder_config = op_rbuilder_config.with_flashblocks_port(flashblocks_port);
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
            framework: framework,
            builder_auth_rpc_port,
            builder_http_port,
            validator_auth_rpc_port,
            builder_log_path,
            chain_block_time: self.chain_block_time,
        })
    }
}

pub struct TestHarness {
    framework: IntegrationFramework,
    builder_auth_rpc_port: u16,
    builder_http_port: u16,
    validator_auth_rpc_port: u16,
    builder_log_path: PathBuf,
    chain_block_time: Option<u64>,
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

        let mut generator = BlockGenerator::new(
            engine_api,
            validation_api,
            false,
            self.chain_block_time.map_or(1, |time| time / 1000), // in seconds
            None,
        );
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

    pub async fn check_tx_in_pool(&self, tx_hash: B256) -> eyre::Result<TransactionStatus> {
        let pool_inspect = self
            .provider()
            .expect("provider not available")
            .txpool_content()
            .await?;

        let is_pending = pool_inspect.pending.iter().any(|pending_account_map| {
            pending_account_map
                .1
                .iter()
                .any(|(_, tx)| tx.as_recovered().hash() == *tx_hash)
        });
        if is_pending {
            return Ok(TransactionStatus::Pending);
        }

        let is_queued = pool_inspect.queued.iter().any(|queued_account_map| {
            queued_account_map
                .1
                .iter()
                .any(|(_, tx)| tx.as_recovered().hash() == *tx_hash)
        });
        if is_queued {
            return Ok(TransactionStatus::Queued);
        }

        // check that the builder emitted logs for the reverted transactions with the monitoring logic
        // this will tell us whether the builder dropped the transaction
        // TODO: this is not ideal, lets find a different way to detect this
        // Each time a transaction is dropped, it emits a log like this
        // Note that this does not tell us the reason why the transaction was dropped. Ideally
        // we should know it at this point.
        // 'Transaction event received target="monitoring" tx_hash="<tx_hash>" kind="discarded"'
        let builder_logs = std::fs::read_to_string(&self.builder_log_path)?;
        let txn_log = format!(
            "Transaction event received target=\"monitoring\" tx_hash=\"{}\" kind=\"discarded\"",
            tx_hash,
        );
        if builder_logs.contains(txn_log.as_str()) {
            return Ok(TransactionStatus::Dropped);
        }

        Ok(TransactionStatus::NotFound)
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        for service in &mut self.framework.services {
            let res = service.stop();
            if let Err(e) = res {
                println!("Failed to stop service: {}", e);
            }
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    NotFound,
    Pending,
    Queued,
    Dropped,
}

impl TransactionStatus {
    pub fn is_pending(&self) -> bool {
        matches!(self, TransactionStatus::Pending)
    }

    pub fn is_queued(&self) -> bool {
        matches!(self, TransactionStatus::Queued)
    }

    pub fn is_dropped(&self) -> bool {
        matches!(self, TransactionStatus::Dropped)
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
