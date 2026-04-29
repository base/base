//! Full challenger service lifecycle.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_provider::{Provider, ProviderBuilder};
use base_balance_monitor::BalanceMonitorLayer;
use base_cli_utils::RuntimeManager;
use base_health::HealthServer;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient,
};
use base_proof_rpc::{L1Client, L1ClientConfig, L2Client, L2ClientConfig};
use base_runtime::TokioRuntime;
use base_tx_manager::{BaseTxMetrics, SimpleTxManager};
use base_zk_client::{ZkProofClient, ZkProofClientConfig};
use eyre::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    BondManager, ChallengeSubmitter, ChallengerConfig, ChallengerMetrics, Driver, DriverComponents,
    DriverConfig, GameScanner, OutputValidator, ScannerConfig,
};

/// Top-level challenger service.
#[derive(Debug)]
pub struct ChallengerService;

impl ChallengerService {
    /// Runs the full challenger service lifecycle.
    ///
    /// # Lifecycle
    ///
    /// 1. Install TLS provider
    /// 2. Create the cancellation token and signal handler
    /// 3. Create L1 provider, tx-manager, and challenge submitter
    /// 4. Create contract clients and read onchain config
    /// 5. Create L2 and ZK clients
    /// 6. Assemble scanner, validator, and driver
    /// 7. Start health HTTP server
    /// 8. Start driver loop
    /// 9. Wait for shutdown signal
    /// 10. Graceful shutdown
    ///
    /// # Errors
    ///
    /// Returns an error if RPC clients cannot connect or onchain
    /// configuration is invalid.
    pub async fn run(config: ChallengerConfig) -> Result<()> {
        // ── 1. Install TLS provider ──────────────────────────────────────────
        // Install the default rustls CryptoProvider before any TLS connections are created.
        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Challenger starting");

        // ── 2. Cancellation token and signal handler ─────────────────────────
        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        // ── 3. Construct tx-manager and challenge submitter ──────────────────
        let signer_config = config.signing;
        let sender_addr = signer_config.address();
        let l1_rpc_url = config.l1_eth_rpc.as_ref().clone();
        let l1_provider = if config.metrics.enabled {
            let (layer, mut balance_rx) = BalanceMonitorLayer::new(
                sender_addr,
                cancel.clone(),
                BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
            );
            let provider = ProviderBuilder::new().layer(layer).connect_http(l1_rpc_url.clone());
            tokio::spawn(async move {
                while balance_rx.changed().await.is_ok() {
                    ChallengerMetrics::account_balance_wei()
                        .set(f64::from(*balance_rx.borrow_and_update()));
                }
            });
            info!(%sender_addr, "Balance monitor started");
            provider
        } else {
            ProviderBuilder::new().connect_http(l1_rpc_url.clone())
        };
        let chain_id = l1_provider
            .get_chain_id()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch L1 chain ID: {e}"))?;
        let tx_manager = SimpleTxManager::new(
            l1_provider,
            signer_config,
            config.tx_manager,
            chain_id,
            Arc::new(BaseTxMetrics::new("challenger")),
        )
        .await
        .map_err(|e| eyre::eyre!("failed to construct tx manager: {e}"))?;
        let submitter = ChallengeSubmitter::new(tx_manager);

        // ── 4. Contract clients and onchain config ───────────────────────────
        let factory_client = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory_addr,
            l1_rpc_url.clone(),
        )?;
        info!(
            address = %config.dispute_game_factory_addr,
            "DisputeGameFactory client initialized"
        );

        let verifier_client = AggregateVerifierContractClient::new(l1_rpc_url.clone())?;

        let factory_client = Arc::new(factory_client);
        let verifier_client: Arc<dyn AggregateVerifierClient> = Arc::new(verifier_client);

        // ── 5. L2 client ─────────────────────────────────────────────────────
        let l2_config = L2ClientConfig::new(config.l2_eth_rpc.as_ref().clone());
        let l2_client = Arc::new(L2Client::new(l2_config)?);
        info!(endpoint = %config.l2_eth_rpc, "L2 client initialized");

        // ── 6. ZK proof client ───────────────────────────────────────────────
        let zk_config = ZkProofClientConfig {
            endpoint: config.zk_rpc_url.as_ref().clone(),
            connect_timeout: config.zk_connect_timeout,
            request_timeout: config.zk_request_timeout,
        };
        let zk_client = Arc::new(ZkProofClient::new(&zk_config)?);
        info!(endpoint = %config.zk_rpc_url, "ZK proof client initialized");

        // ── 6b. TEE proof client (optional) ─────────────────────────────────
        let tee = if let Some(ref tee_url) = config.tee_rpc_url {
            let request_timeout = config.tee_request_timeout.ok_or_else(|| {
                eyre::eyre!("tee_request_timeout must be set when tee_rpc_url is configured")
            })?;
            let client = jsonrpsee::http_client::HttpClientBuilder::default()
                .request_timeout(request_timeout)
                .build(tee_url.as_str())
                .map_err(|e| eyre::eyre!("failed to create TEE RPC client: {e}"))?;
            info!(endpoint = %tee_url, "TEE proof client initialized");
            let l1_config = L1ClientConfig::new(l1_rpc_url.clone());
            let l1_client = L1Client::new(l1_config)
                .map_err(|e| eyre::eyre!("failed to create TEE L1 client: {e}"))?;
            Some(crate::TeeConfig {
                provider: Arc::new(client),
                l1_head_provider: Arc::new(l1_client),
                request_timeout,
            })
        } else {
            info!("TEE proof sourcing disabled (no --tee-rpc-url)");
            None
        };

        // ── 7. Assemble scanner, validator, and driver ───────────────────────
        let scanner_config = ScannerConfig { lookback_games: config.lookback_games };

        // ── 7b. Bond manager (optional) ─────────────────────────────────────
        let bond_manager = if !config.bond_claim_addresses.is_empty() {
            let mut bm = BondManager::new(
                config.bond_claim_addresses,
                l1_rpc_url,
                Arc::clone(&factory_client) as Arc<dyn DisputeGameFactoryClient>,
                config.lookback_games,
                config.bond_discovery_interval,
                TokioRuntime::new(),
            );
            bm.set_anchor_update_retention(config.anchor_update_retention);
            info!("starting bond recovery scan");
            if let Err(e) = bm.startup_scan(&*verifier_client).await {
                // On failure `bond_scan_head` stays at 0, so
                // `discover_claimable_games` will progressively scan the
                // factory over multiple ticks (capped at `lookback` games
                // per tick). This is intentional: progressive catch-up
                // is preferable to disabling bond claiming entirely.
                warn!(error = %e, "bond startup scan failed, continuing without recovery");
            }
            info!(tracked = bm.tracked_count(), "bond manager ready");
            Some(bm)
        } else {
            info!("bond claiming disabled (no --bond-claim-addresses)");
            None
        };

        let scanner =
            GameScanner::new(factory_client, Arc::clone(&verifier_client), scanner_config);

        let validator = OutputValidator::new(l2_client);

        // ── 8. Start health HTTP server ──────────────────────────────────────
        let ready = Arc::new(AtomicBool::new(false));
        let health_handle = {
            let addr = config.health_addr;
            let ready_flag = Arc::clone(&ready);
            let health_cancel = cancel.clone();
            tokio::spawn(async move { HealthServer::serve(addr, ready_flag, health_cancel).await })
        };

        // ── 9. Run driver ────────────────────────────────────────────────────
        let driver_config = DriverConfig {
            poll_interval: config.poll_interval,
            cancel: cancel.child_token(),
            ready: Arc::clone(&ready),
        };
        let driver = Driver::new(
            driver_config,
            DriverComponents {
                scanner,
                validator,
                zk_prover: zk_client,
                submitter,
                tee,
                verifier_client,
                bond_manager,
            },
        );

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
