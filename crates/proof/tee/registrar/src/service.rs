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

impl RegistrarConfig {
    /// Runs the full registrar service lifecycle.
    ///
    /// # Errors
    ///
    /// Returns an error if service initialization fails or the registration
    /// driver exits with an error.
    pub async fn run(self) -> eyre::Result<()> {
        self.log_config.init_tracing_subscriber()?;

        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Registrar starting");

        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        self.metrics_config
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
                RegistrarMetrics::up().set(1.0);
            })
            .wrap_err("failed to install Prometheus recorder")?;

        if self.metrics_config.enabled {
            Self::spawn_balance_monitor(
                self.l1_rpc_url.clone(),
                self.signing.address(),
                cancel.clone(),
                |balance| RegistrarMetrics::account_balance_wei().set(balance),
            );
            Self::spawn_balance_monitor(
                self.boundless_prover.rpc_url.clone(),
                self.boundless_prover.signer.address(),
                cancel.clone(),
                |balance| RegistrarMetrics::boundless_balance_wei().set(balance),
            );
        }

        let provider = ProviderBuilder::new().connect_http(self.l1_rpc_url.clone());
        let l1_chain_id = provider.get_chain_id().await.wrap_err("failed to fetch L1 chain ID")?;
        let tx_manager = SimpleTxManager::new(
            provider,
            self.signing,
            self.tx_manager_config,
            l1_chain_id,
            Arc::new(BaseTxMetrics::new("registrar")),
        )
        .await?;

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(self.aws_region))
            .load()
            .await;
        let discovery = AwsTargetGroupDiscovery::new(
            aws_sdk_elasticloadbalancingv2::Client::new(&aws_config),
            aws_sdk_ec2::Client::new(&aws_config),
            self.target_group_arn,
            self.prover_port,
        );

        let registry =
            RegistryContractClient::new(self.tee_prover_registry_address, self.l1_rpc_url.clone());

        let ready = Arc::new(AtomicBool::new(false));
        let health_handle =
            tokio::spawn(HealthServer::serve(self.health_addr, Arc::clone(&ready), cancel.clone()));

        ready.store(true, Ordering::Relaxed);

        let signer_manager = Arc::new(SignerManager::new(
            self.boundless_prover,
            registry,
            tx_manager.clone(),
            SignerManagerConfig {
                registry_address: self.tee_prover_registry_address,
                max_concurrency: self.max_concurrency,
                max_tx_retries: self.max_tx_retries,
                tx_retry_delay: self.tx_retry_delay,
            },
        ));
        let cert_manager = self
            .crl_nitro_verifier_address
            .map(|nitro_verifier_address| {
                CertManager::new(
                    Duration::from_secs(DEFAULT_CRL_FETCH_TIMEOUT_SECS),
                    Box::new(NitroVerifierContractClient::new(
                        nitro_verifier_address,
                        self.l1_rpc_url,
                    )),
                    tx_manager,
                )
            })
            .transpose()?;
        let driver = RegistrationDriver::new(
            discovery,
            ProverClient::new(self.prover_timeout),
            DriverConfig {
                poll_interval: self.poll_interval,
                cancel: cancel.clone(),
                max_concurrency: self.max_concurrency,
                unhealthy_registration_window: self.unhealthy_registration_window,
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
        driver_result.map_err(Into::into)
    }

    /// Starts a balance monitor for registrar account metrics.
    pub fn spawn_balance_monitor(
        rpc_url: Url,
        address: Address,
        cancel: CancellationToken,
        set_metric: impl Fn(f64) + Send + 'static,
    ) {
        tokio::spawn(async move {
            let provider = ProviderBuilder::new().connect_http(rpc_url);
            let mut interval = tokio::time::interval(Duration::from_secs(12));
            interval.tick().await;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => match provider.get_balance(address).await {
                        Ok(balance) => set_metric(f64::from(balance)),
                        Err(e) => warn!(error = %e, %address, "failed to fetch account balance"),
                    },
                }
            }
        });
        info!(%address, "balance monitor started");
    }
}
