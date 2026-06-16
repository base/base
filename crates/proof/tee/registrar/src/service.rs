//! Service lifecycle for the prover registrar.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use base_balance_monitor::BalanceMonitorLayer;
use base_cli_utils::RuntimeManager;
use base_health::HealthServer;
use base_proof_tee_nitro_attestation_prover::BoundlessProver;
use base_tx_manager::{BaseTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use eyre::WrapErr;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    AwsTargetGroupDiscovery, CertManager, DEFAULT_CRL_FETCH_TIMEOUT_SECS, DriverConfig,
    NitroVerifierContractClient, ProverClient, RegistrarMetrics, RegistrationDriver,
    RegistryContractClient, SignerManager, SignerManagerConfig,
};

/// Configuration needed to run the registrar service.
#[derive(Debug)]
pub struct RegistrarConfig {
    /// L1 Ethereum RPC endpoint.
    pub l1_rpc_url: Url,
    /// `TEEProverRegistry` contract address on L1.
    pub tee_prover_registry_address: Address,
    /// AWS ALB target group ARN for prover instance discovery.
    pub target_group_arn: String,
    /// AWS region.
    pub aws_region: String,
    /// JSON-RPC port to poll on each prover instance.
    pub prover_port: u16,
    /// L1 transaction signer.
    pub signing: SignerConfig,
    /// Transaction manager configuration.
    pub tx_manager_config: TxManagerConfig,
    /// Boundless prover client configuration.
    pub boundless_prover: BoundlessProver,
    /// Interval between discovery and registration poll cycles.
    pub poll_interval: Duration,
    /// Timeout for JSON-RPC calls to prover instances.
    pub prover_timeout: Duration,
    /// Maximum number of instances to process concurrently.
    pub max_concurrency: usize,
    /// Maximum number of transaction submission retries for transient errors.
    pub max_tx_retries: u32,
    /// Delay between transaction submission retries.
    pub tx_retry_delay: Duration,
    /// Grace window for registering recently launched unhealthy instances.
    pub unhealthy_registration_window: Duration,
    /// Optional Nitro verifier address for CRL checks.
    pub crl_nitro_verifier_address: Option<Address>,
    /// Health server bind address.
    pub health_addr: SocketAddr,
    /// Logging configuration.
    pub log_config: base_cli_utils::LogConfig,
    /// Metrics configuration.
    pub metrics_config: base_cli_utils::MetricsConfig,
}

/// Top-level registrar service.
#[derive(Debug)]
pub struct RegistrarService;

impl RegistrarService {
    /// Runs the full registrar service lifecycle.
    ///
    /// # Errors
    ///
    /// Returns an error if service initialization fails or the registration
    /// driver exits with an error.
    pub async fn run(config: RegistrarConfig) -> eyre::Result<()> {
        config.log_config.init_tracing_subscriber()?;

        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Registrar starting");

        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        config
            .metrics_config
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
                RegistrarMetrics::up().set(1.0);
            })
            .wrap_err("failed to install Prometheus recorder")?;

        let provider = if config.metrics_config.enabled {
            let provider = ProviderBuilder::new()
                .layer(Self::balance_monitor_layer(
                    config.signing.address(),
                    cancel.clone(),
                    |balance| RegistrarMetrics::account_balance_wei().set(balance),
                ))
                .connect_http(config.l1_rpc_url.clone());

            ProviderBuilder::new()
                .layer(Self::balance_monitor_layer(
                    config.boundless_prover.signer.address(),
                    cancel.clone(),
                    |balance| RegistrarMetrics::boundless_balance_wei().set(balance),
                ))
                .connect_http(config.boundless_prover.rpc_url.clone());

            provider
        } else {
            ProviderBuilder::new().connect_http(config.l1_rpc_url.clone())
        };

        let l1_chain_id = provider.get_chain_id().await.wrap_err("failed to fetch L1 chain ID")?;
        let tx_manager = SimpleTxManager::new(
            provider,
            config.signing,
            config.tx_manager_config,
            l1_chain_id,
            Arc::new(BaseTxMetrics::new("registrar")),
        )
        .await?;

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.aws_region))
            .load()
            .await;
        let discovery = AwsTargetGroupDiscovery::new(
            aws_sdk_elasticloadbalancingv2::Client::new(&aws_config),
            aws_sdk_ec2::Client::new(&aws_config),
            config.target_group_arn,
            config.prover_port,
        );

        let registry = RegistryContractClient::new(
            config.tee_prover_registry_address,
            config.l1_rpc_url.clone(),
        );

        let ready = Arc::new(AtomicBool::new(false));
        let health_handle = tokio::spawn(HealthServer::serve(
            config.health_addr,
            Arc::clone(&ready),
            cancel.clone(),
        ));

        let signer_client = ProverClient::new(config.prover_timeout);

        ready.store(true, Ordering::Relaxed);

        let signer_manager = Arc::new(SignerManager::new(
            config.boundless_prover,
            registry,
            tx_manager.clone(),
            SignerManagerConfig {
                registry_address: config.tee_prover_registry_address,
                max_concurrency: config.max_concurrency,
                max_tx_retries: config.max_tx_retries,
                tx_retry_delay: config.tx_retry_delay,
            },
        ));
        let cert_manager = if let Some(nitro_verifier_address) = config.crl_nitro_verifier_address {
            Some(CertManager::new(
                Duration::from_secs(DEFAULT_CRL_FETCH_TIMEOUT_SECS),
                Box::new(NitroVerifierContractClient::new(
                    nitro_verifier_address,
                    config.l1_rpc_url,
                )),
                tx_manager,
            )?)
        } else {
            None
        };
        let driver = RegistrationDriver::new(
            discovery,
            signer_client,
            DriverConfig {
                poll_interval: config.poll_interval,
                cancel: cancel.clone(),
                max_concurrency: config.max_concurrency,
                unhealthy_registration_window: config.unhealthy_registration_window,
            },
            cert_manager,
            signer_manager,
        );
        let driver_result = driver.run().await;
        cancel.cancel();

        info!("Driver stopped, shutting down...");
        ready.store(false, Ordering::Relaxed);
        RegistrarMetrics::up().set(0.0);

        match health_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "Health server error during shutdown"),
            Err(e) => warn!(error = %e, "Health server task panicked"),
        }

        signal_handle.abort();

        info!("Service stopped");
        driver_result?;
        Ok(())
    }

    /// Creates a provider layer that reports account balance metrics.
    fn balance_monitor_layer(
        address: Address,
        cancel: CancellationToken,
        set_metric: impl Fn(f64) + Send + 'static,
    ) -> BalanceMonitorLayer {
        let (layer, mut balance_rx) =
            BalanceMonitorLayer::new(address, cancel, BalanceMonitorLayer::DEFAULT_POLL_INTERVAL);
        tokio::spawn(async move {
            while balance_rx.changed().await.is_ok() {
                set_metric(f64::from(*balance_rx.borrow_and_update()));
            }
        });
        info!(%address, "balance monitor started");
        layer
    }
}
