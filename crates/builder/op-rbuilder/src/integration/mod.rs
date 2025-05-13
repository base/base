use alloy_consensus::TxEip1559;
use alloy_eips::{eip1559::MIN_PROTOCOL_BASE_FEE, eip2718::Encodable2718, BlockNumberOrTag};
use alloy_primitives::hex;
use alloy_provider::{
    Identity, PendingTransactionBuilder, Provider, ProviderBuilder, RootProvider,
};
use op_alloy_consensus::OpTypedTransaction;
use op_alloy_network::Optimism;
use op_rbuilder::OpRbuilderConfig;
use op_reth::OpRethConfig;
use parking_lot::Mutex;
use std::{
    cmp::max,
    collections::HashSet,
    fs::{File, OpenOptions},
    future::Future,
    io,
    io::prelude::*,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::LazyLock,
    time::{Duration, SystemTime},
};
use time::{format_description, OffsetDateTime};
use tokio::time::sleep;
use uuid::Uuid;

use crate::{
    tester::{BlockGenerator, EngineApi},
    tx_signer::Signer,
};

/// Default JWT token for testing purposes
pub const DEFAULT_JWT_TOKEN: &str =
    "688f5d737bad920bdfb2fc2f488d6b6209eebda1dae949a8de91398d932c517a";

mod integration_test;
pub mod op_rbuilder;
pub mod op_reth;

#[derive(Debug)]
pub enum IntegrationError {
    SpawnError,
    BinaryNotFound,
    SetupError,
    LogError,
    ServiceAlreadyRunning,
}

pub struct ServiceInstance {
    process: Option<Child>,
    pub log_path: PathBuf,
}

pub struct IntegrationFramework {
    test_dir: PathBuf,
    services: Vec<ServiceInstance>,
}

pub trait Service {
    /// Configure and return the command to run the service
    fn command(&self) -> Command;

    /// Return a future that resolves when the service is ready
    fn ready(&self, log_path: &Path) -> impl Future<Output = Result<(), IntegrationError>> + Send;
}

/// Helper function to poll logs periodically
pub async fn poll_logs(
    log_path: &Path,
    pattern: &str,
    interval: Duration,
    timeout: Duration,
) -> Result<(), IntegrationError> {
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            return Err(IntegrationError::SpawnError);
        }

        let mut file = File::open(log_path).map_err(|_| IntegrationError::LogError)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|_| IntegrationError::LogError)?;

        if contents.contains(pattern) {
            return Ok(());
        }

        sleep(interval).await;
    }
}

impl ServiceInstance {
    pub fn new(name: String, test_dir: PathBuf) -> Self {
        let log_path = test_dir.join(format!("{name}.log"));
        Self {
            process: None,
            log_path,
        }
    }

    pub fn start(&mut self, command: Command) -> Result<(), IntegrationError> {
        if self.process.is_some() {
            return Err(IntegrationError::ServiceAlreadyRunning);
        }

        let log = open_log_file(&self.log_path)?;
        let stdout = log.try_clone().map_err(|_| IntegrationError::LogError)?;
        let stderr = log.try_clone().map_err(|_| IntegrationError::LogError)?;

        let mut cmd = command;
        cmd.stdout(stdout).stderr(stderr);

        let child = match cmd.spawn() {
            Ok(child) => Ok(child),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => Err(IntegrationError::BinaryNotFound),
                _ => Err(IntegrationError::SpawnError),
            },
        }?;

        self.process = Some(child);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), IntegrationError> {
        if let Some(mut process) = self.process.take() {
            process.kill().map_err(|_| IntegrationError::SpawnError)?;
        }
        Ok(())
    }

    /// Start a service using its configuration and wait for it to be ready
    pub async fn start_with_config<T: Service>(
        &mut self,
        config: &T,
    ) -> Result<(), IntegrationError> {
        self.start(config.command())?;
        config.ready(&self.log_path).await?;
        Ok(())
    }

    pub async fn find_log_line(&self, pattern: &str) -> eyre::Result<()> {
        let mut file =
            File::open(&self.log_path).map_err(|_| eyre::eyre!("Failed to open log file"))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|_| eyre::eyre!("Failed to read log file"))?;

        if contents.contains(pattern) {
            Ok(())
        } else {
            Err(eyre::eyre!("Pattern not found in log file: {}", pattern))
        }
    }
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
    ) -> Result<&mut ServiceInstance, IntegrationError> {
        let service = self.create_service(name)?;
        service.start_with_config(config).await?;
        Ok(service)
    }

    pub fn create_service(&mut self, name: &str) -> Result<&mut ServiceInstance, IntegrationError> {
        let service = ServiceInstance::new(name.to_string(), self.test_dir.clone());
        self.services.push(service);
        Ok(self.services.last_mut().unwrap())
    }
}

fn open_log_file(path: &PathBuf) -> Result<File, IntegrationError> {
    let prefix = path.parent().unwrap();
    std::fs::create_dir_all(prefix).map_err(|_| IntegrationError::LogError)?;

    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|_| IntegrationError::LogError)
}

impl Drop for IntegrationFramework {
    fn drop(&mut self) {
        for service in &mut self.services {
            let _ = service.stop();
        }
    }
}

const BUILDER_PRIVATE_KEY: &str =
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

pub struct TestHarnessBuilder {
    name: String,
    use_revert_protection: bool,
}

impl TestHarnessBuilder {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            use_revert_protection: false,
        }
    }

    pub fn with_revert_protection(mut self) -> Self {
        self.use_revert_protection = true;
        self
    }

    pub async fn build(self) -> eyre::Result<TestHarness> {
        let mut framework = IntegrationFramework::new(&self.name).unwrap();

        // we are going to use a genesis file pre-generated before the test
        let mut genesis_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        genesis_path.push("../../genesis.json");
        assert!(genesis_path.exists());

        // create the builder
        let builder_data_dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let builder_auth_rpc_port = get_available_port();
        let builder_http_port = get_available_port();
        let op_rbuilder_config = OpRbuilderConfig::new()
            .chain_config_path(genesis_path.clone())
            .data_dir(builder_data_dir)
            .auth_rpc_port(builder_auth_rpc_port)
            .network_port(get_available_port())
            .http_port(builder_http_port)
            .with_builder_private_key(BUILDER_PRIVATE_KEY)
            .with_revert_protection(self.use_revert_protection);

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
    #[allow(dead_code)] // I think this is due to some feature flag conflicts
    builder_log_path: PathBuf,
}

impl TestHarness {
    pub async fn send_valid_transaction(
        &self,
    ) -> eyre::Result<PendingTransactionBuilder<Optimism>> {
        // Get builder's address
        let known_wallet = Signer::try_from_secret(BUILDER_PRIVATE_KEY.parse()?)?;
        let builder_address = known_wallet.address;

        let url = format!("http://localhost:{}", self.builder_http_port);
        let provider =
            ProviderBuilder::<Identity, Identity, Optimism>::default().on_http(url.parse()?);

        // Get current nonce includeing the ones from the txpool
        let nonce = provider
            .get_transaction_count(builder_address)
            .pending()
            .await?;

        let latest_block = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .unwrap();

        let base_fee = max(
            latest_block.header.base_fee_per_gas.unwrap(),
            MIN_PROTOCOL_BASE_FEE,
        );

        // Transaction from builder should succeed
        let tx_request = OpTypedTransaction::Eip1559(TxEip1559 {
            chain_id: 901,
            nonce,
            gas_limit: 210000,
            max_fee_per_gas: base_fee.into(),
            ..Default::default()
        });
        let signed_tx = known_wallet.sign_tx(tx_request)?;
        let pending_tx = provider
            .send_raw_transaction(signed_tx.encoded_2718().as_slice())
            .await?;

        Ok(pending_tx)
    }

    pub async fn send_revert_transaction(
        &self,
    ) -> eyre::Result<PendingTransactionBuilder<Optimism>> {
        // TODO: Merge this with send_valid_transaction
        // Get builder's address
        let known_wallet = Signer::try_from_secret(BUILDER_PRIVATE_KEY.parse()?)?;
        let builder_address = known_wallet.address;

        let url = format!("http://localhost:{}", self.builder_http_port);
        let provider =
            ProviderBuilder::<Identity, Identity, Optimism>::default().on_http(url.parse()?);

        // Get current nonce includeing the ones from the txpool
        let nonce = provider
            .get_transaction_count(builder_address)
            .pending()
            .await?;

        let latest_block = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .unwrap();

        let base_fee = max(
            latest_block.header.base_fee_per_gas.unwrap(),
            MIN_PROTOCOL_BASE_FEE,
        );

        // Transaction from builder should succeed
        let tx_request = OpTypedTransaction::Eip1559(TxEip1559 {
            chain_id: 901,
            nonce,
            gas_limit: 210000,
            max_fee_per_gas: base_fee.into(),
            input: hex!("60006000fd").into(), // PUSH1 0x00 PUSH1 0x00 REVERT
            ..Default::default()
        });
        let signed_tx = known_wallet.sign_tx(tx_request)?;
        let pending_tx = provider
            .send_raw_transaction(signed_tx.encoded_2718().as_slice())
            .await?;

        Ok(pending_tx)
    }

    pub fn provider(&self) -> eyre::Result<RootProvider<Optimism>> {
        let url = format!("http://localhost:{}", self.builder_http_port);
        let provider =
            ProviderBuilder::<Identity, Identity, Optimism>::default().on_http(url.parse()?);

        Ok(provider)
    }

    pub async fn block_generator(&self) -> eyre::Result<BlockGenerator> {
        let engine_api = EngineApi::new_with_port(self.builder_auth_rpc_port).unwrap();
        let validation_api = Some(EngineApi::new_with_port(self.validator_auth_rpc_port).unwrap());

        let mut generator = BlockGenerator::new(engine_api, validation_api, false, 1, None);
        generator.init().await?;

        Ok(generator)
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
