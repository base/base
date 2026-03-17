//! Full challenger service lifecycle.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_provider::{Provider, RootProvider};
use base_cli_utils::RuntimeManager;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryContractClient,
};
use base_proof_rpc::{L2Client, L2ClientConfig};
use base_tx_manager::{NoopTxMetrics, SimpleTxManager};
use base_zk_client::{ZkProofClient, ZkProofClientConfig};
use eyre::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    ChallengeSubmitter, ChallengerConfig, Driver, DriverConfig, GameScanner, OutputValidator,
    ScannerConfig,
};

/// Top-level challenger service.
#[derive(Debug)]
pub struct ChallengerService;

impl ChallengerService {
    /// Runs the full challenger service lifecycle.
    ///
    /// # Lifecycle
    ///
    /// 1. Initialise logging, TLS, and metrics
    /// 2. Create L1 provider, tx-manager, and challenge submitter
    /// 3. Create contract clients and read onchain config
    /// 4. Create L2 and ZK clients
    /// 5. Assemble scanner, validator, and driver
    /// 6. Start health HTTP server
    /// 7. Start driver loop
    /// 8. Wait for shutdown signal
    /// 9. Graceful shutdown
    ///
    /// # Errors
    ///
    /// Returns an error if tracing initialisation fails, the Prometheus
    /// recorder cannot be installed, RPC clients cannot connect, or
    /// onchain configuration is invalid.
    pub async fn run(config: ChallengerConfig) -> Result<()> {
        config.log.init_tracing_subscriber()?;

        // Install the default rustls CryptoProvider before any TLS connections are created.
        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Challenger starting");

        // ── 1. Cancellation token and signal handler ─────────────────────────
        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        // ── 2. Metrics recorder (if enabled) ─────────────────────────────────
        config
            .metrics
            .init()
            .map_err(|e| eyre::eyre!("failed to install Prometheus recorder: {e}"))?;

        crate::ChallengerMetrics::record_startup(env!("CARGO_PKG_VERSION"));

        // ── 3. Construct tx-manager and challenge submitter ──────────────────
        let l1_provider = RootProvider::new_http(config.l1_eth_rpc.as_ref().clone());
        let signer_config = config.signing;
        let chain_id = l1_provider
            .get_chain_id()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch L1 chain ID: {e}"))?;
        let tx_manager = SimpleTxManager::new(
            l1_provider,
            signer_config,
            config.tx_manager,
            chain_id,
            Arc::new(NoopTxMetrics),
        )
        .await
        .map_err(|e| eyre::eyre!("failed to construct tx manager: {e}"))?;
        let submitter = ChallengeSubmitter::new(tx_manager);

        // ── 4. Contract clients and onchain config ───────────────────────────
        let factory_client = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory_addr,
            config.l1_eth_rpc.as_ref().clone(),
        )?;
        info!(
            address = %config.dispute_game_factory_addr,
            "DisputeGameFactory client initialized"
        );

        let verifier_client =
            AggregateVerifierContractClient::new(config.l1_eth_rpc.as_ref().clone())?;

        let factory_client = Arc::new(factory_client);
        let verifier_client: Arc<dyn AggregateVerifierClient> = Arc::new(verifier_client);

        // ── 5. L2 client ─────────────────────────────────────────────────────
        let l2_config = L2ClientConfig::new(config.l2_eth_rpc.as_ref().clone());
        let l2_client = Arc::new(L2Client::new(l2_config)?);
        info!(endpoint = %config.l2_eth_rpc, "L2 client initialized");

        // ── 6. ZK proof client ───────────────────────────────────────────────
        let zk_config = ZkProofClientConfig {
            endpoint: config.zk_proof_service_endpoint.as_ref().clone(),
            connect_timeout: config.zk_connect_timeout,
            request_timeout: config.zk_request_timeout,
        };
        let zk_client = Arc::new(ZkProofClient::new(&zk_config)?);
        info!(endpoint = %config.zk_proof_service_endpoint, "ZK proof client initialized");

        // ── 7. Assemble scanner, validator, and driver ───────────────────────
        let scanner_config = ScannerConfig { lookback_games: config.lookback_games };
        let scanner =
            GameScanner::new(factory_client, Arc::clone(&verifier_client), scanner_config);

        let validator = OutputValidator::new(l2_client);

        // ── 8. Start health HTTP server ──────────────────────────────────────
        let ready = Arc::new(AtomicBool::new(false));
        let health_handle = {
            let addr = config.health_addr;
            let ready_flag = Arc::clone(&ready);
            let health_cancel = cancel.clone();
            tokio::spawn(async move {
                crate::HealthServer::serve(addr, ready_flag, health_cancel).await
            })
        };

        // ── 9. Run driver ────────────────────────────────────────────────────
        let driver_config = DriverConfig {
            poll_interval: config.poll_interval,
            cancel: cancel.child_token(),
            ready: Arc::clone(&ready),
        };
        let driver =
            Driver::new(driver_config, scanner, validator, zk_client, submitter, verifier_client);

        // Drop guard ensures child tasks are cancelled even if the driver panics.
        let cancel_guard = cancel.clone().drop_guard();
        driver.run().await;
        drop(cancel_guard);

        // ── 10. Graceful shutdown ────────────────────────────────────────────
        info!("Driver stopped, shutting down...");
        ready.store(false, Ordering::SeqCst);

        match health_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "Health server error during shutdown"),
            Err(e) => warn!(error = %e, "Health server task panicked"),
        }

        signal_handle.abort();
        match signal_handle.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => warn!(error = %e, "Signal handler task panicked"),
        }

        info!("Service stopped");
        Ok(())
    }
}
