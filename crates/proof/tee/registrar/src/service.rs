//! Service lifecycle for the prover registrar.

use std::{
    fmt,
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
use base_proof_contracts::TEEProverRegistryContractClient;
use base_proof_tee_nitro_attestation_prover::BoundlessProver;
use base_tx_manager::{BaseTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    AwsTargetGroupDiscovery, CertManager, DriverConfig, NitroVerifierContractClient, ProverClient,
    RegistrarError, RegistrarMetrics, RegistrationDriver, Result, SignerManager,
};

const CRL_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Configuration needed to run the registrar service.
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
    /// Optional Nitro verifier address for CRL checks. Providing this enables CRL checks.
    pub crl_nitro_verifier_address: Option<Address>,
    /// Health server bind address.
    pub health_addr: SocketAddr,
    /// Logging configuration.
    pub log_config: base_cli_utils::LogConfig,
    /// Metrics configuration.
    pub metrics_config: base_cli_utils::MetricsConfig,
}

impl fmt::Debug for RegistrarConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrarConfig")
            .field("l1_rpc_url", &self.l1_rpc_url.origin().unicode_serialization())
            .field("tee_prover_registry_address", &self.tee_prover_registry_address)
            .field("target_group_arn", &self.target_group_arn)
            .field("aws_region", &self.aws_region)
            .field("prover_port", &self.prover_port)
            .field("signing", &self.signing)
            .field("tx_manager_config", &self.tx_manager_config)
            .field("boundless_prover", &self.boundless_prover)
            .field("poll_interval", &self.poll_interval)
            .field("prover_timeout", &self.prover_timeout)
            .field("max_concurrency", &self.max_concurrency)
            .field("max_tx_retries", &self.max_tx_retries)
            .field("tx_retry_delay", &self.tx_retry_delay)
            .field("unhealthy_registration_window", &self.unhealthy_registration_window)
            .field("crl_nitro_verifier_address", &self.crl_nitro_verifier_address)
            .field("health_addr", &self.health_addr)
            .field("log_config", &self.log_config)
            .field("metrics_config", &self.metrics_config)
            .finish()
    }
}

impl RegistrarConfig {
    /// Runs the full registrar service lifecycle.
    ///
    /// # Errors
    ///
    /// Returns an error if service initialization fails or the registration
    /// driver exits with an error.
    pub async fn run(self) -> Result<()> {
        self.log_config
            .init_tracing_subscriber()
            .map_err(|e| RegistrarError::Service(e.to_string()))?;

        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Registrar starting");

        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());
        let mut balance_monitor_handles = Vec::new();

        self.metrics_config
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
                RegistrarMetrics::up().set(1.0);
            })
            .map_err(|e| {
                RegistrarError::Service(format!("failed to install Prometheus recorder: {e}"))
            })?;

        let provider = if self.metrics_config.enabled {
            let account_address = self.signing.address();
            let (layer, mut account_balance_rx) = BalanceMonitorLayer::new(
                account_address,
                cancel.clone(),
                BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
            );
            let account_balance_cancel = cancel.clone();
            balance_monitor_handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = account_balance_cancel.cancelled() => break,
                        changed = account_balance_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            RegistrarMetrics::account_balance_wei()
                                .set(f64::from(*account_balance_rx.borrow_and_update()));
                        }
                    }
                }
            }));

            let boundless_address = self.boundless_prover.signer.address();
            let (boundless_layer, mut boundless_balance_rx) = BalanceMonitorLayer::new(
                boundless_address,
                cancel.clone(),
                BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
            );
            // The balance monitor layer starts polling when it is applied.
            let _boundless_provider = ProviderBuilder::new()
                .layer(boundless_layer)
                .connect_http(self.boundless_prover.rpc_url.clone());
            let boundless_balance_cancel = cancel.clone();
            balance_monitor_handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = boundless_balance_cancel.cancelled() => break,
                        changed = boundless_balance_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            RegistrarMetrics::boundless_balance_wei()
                                .set(f64::from(*boundless_balance_rx.borrow_and_update()));
                        }
                    }
                }
            }));

            ProviderBuilder::new().layer(layer).connect_http(self.l1_rpc_url.clone())
        } else {
            ProviderBuilder::new().connect_http(self.l1_rpc_url.clone())
        };
        let l1_chain_id =
            tokio::time::timeout(self.tx_manager_config.network_timeout, provider.get_chain_id())
                .await
                .map_err(|_| {
                    RegistrarError::Service("failed to fetch L1 chain ID: timed out".into())
                })?
                .map_err(|e| {
                    RegistrarError::Service(format!("failed to fetch L1 chain ID: {e}"))
                })?;
        info!(chain_id = l1_chain_id, "discovered L1 chain ID from provider");
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
        let discovery =
            AwsTargetGroupDiscovery::new(&aws_config, self.target_group_arn, self.prover_port);

        let registry = TEEProverRegistryContractClient::new(
            self.tee_prover_registry_address,
            self.l1_rpc_url.clone(),
        );

        let ready = Arc::new(AtomicBool::new(false));
        let health_handle =
            tokio::spawn(HealthServer::serve(self.health_addr, Arc::clone(&ready), cancel.clone()));

        let signer_manager = Arc::new(SignerManager::new(
            self.boundless_prover,
            registry,
            tx_manager.clone(),
            self.tee_prover_registry_address,
            self.max_concurrency,
            self.max_tx_retries,
            self.tx_retry_delay,
        ));
        let cert_manager = if let Some(nitro_verifier_address) = self.crl_nitro_verifier_address {
            Some(CertManager::new(
                CRL_FETCH_TIMEOUT,
                Box::new(NitroVerifierContractClient::new(nitro_verifier_address, self.l1_rpc_url)),
                tx_manager,
            )?)
        } else {
            None
        };
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
        ready.store(true, Ordering::SeqCst);
        let driver_result = driver.run().await;
        ready.store(false, Ordering::SeqCst);
        cancel.cancel();

        info!("Driver stopped, shutting down...");
        RegistrarMetrics::up().set(0.0);

        match health_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "Health server error during shutdown"),
            Err(e) => warn!(error = %e, "Health server task panicked"),
        }

        for handle in balance_monitor_handles {
            match handle.await {
                Ok(()) => {}
                Err(e) if e.is_cancelled() => {}
                Err(e) => warn!(error = %e, "Balance monitor metrics task panicked"),
            }
        }

        signal_handle.abort();
        match signal_handle.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => warn!(error = %e, "Signal handler task panicked"),
        }

        info!("Service stopped");
        driver_result
    }
}

#[cfg(test)]
mod tests {
    use base_proof_tee_nitro_attestation_prover::BoundlessProverConfig;

    use super::*;
    use crate::test_utils::TEST_REGISTRY_ADDRESS;

    #[test]
    fn debug_redacts_l1_rpc_url_path() {
        let api_key = "SECRET_KEY";
        let config = RegistrarConfig {
            l1_rpc_url: Url::parse(&format!("https://mainnet.infura.io/v3/{api_key}")).unwrap(),
            tee_prover_registry_address: TEST_REGISTRY_ADDRESS,
            target_group_arn: "arn:aws:elasticloadbalancing:us-east-1:123:targetgroup/test/abc"
                .to_string(),
            aws_region: "us-east-1".to_string(),
            prover_port: 8000,
            signing: SignerConfig::local(
                "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
                    .parse()
                    .unwrap(),
            ),
            tx_manager_config: TxManagerConfig::default(),
            boundless_prover: BoundlessProver::new(BoundlessProverConfig {
                rpc_url: Url::parse("https://boundless.example/v3/BOUNDLESS_SECRET").unwrap(),
                signer: "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
                    .parse()
                    .unwrap(),
                verifier_program_url: Url::parse("https://program.example/ipfs/PROGRAM_SECRET")
                    .unwrap(),
                image_id: [0; 8],
                poll_interval: Duration::from_secs(1),
                timeout: Duration::from_secs(1),
                max_recovery_attempts: 1,
                max_attestation_age: Duration::from_secs(1),
                offer_min_price: None,
                offer_max_price: None,
                offer_ramp_up_period_secs: None,
                offer_lock_timeout_secs: None,
                offer_bidding_start_delay_secs: 0,
            }),
            poll_interval: Duration::from_secs(1),
            prover_timeout: Duration::from_secs(1),
            max_concurrency: 1,
            max_tx_retries: 1,
            tx_retry_delay: Duration::from_secs(1),
            unhealthy_registration_window: Duration::from_secs(1),
            crl_nitro_verifier_address: None,
            health_addr: "127.0.0.1:0".parse().unwrap(),
            log_config: base_cli_utils::LogConfig::default(),
            metrics_config: base_cli_utils::MetricsConfig::default(),
        };

        let debug = format!("{config:?}");

        assert!(!debug.contains(api_key), "L1 RPC URL path must not appear in Debug output");
        assert!(debug.contains("mainnet.infura.io"), "L1 RPC host should still be visible");
    }
}
