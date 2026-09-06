//! Full challenger service lifecycle.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use base_balance_monitor::BalanceMonitorLayer;
use base_cli_utils::RuntimeManager;
use base_health::HealthServer;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorStateRegistryClient,
    AnchorStateRegistryContractClient, DisputeGameFactoryClient, DisputeGameFactoryContractClient,
};
use base_proof_rpc::{L1Client, L1ClientConfig, L1Provider, L2Client, L2ClientConfig, L2Provider};
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_runtime::TokioRuntime;
use base_tx_manager::{BaseTxMetrics, SimpleTxManager};
use eyre::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    AnchorUpdater, BondManager, BondManagerConfig, ChallengeSubmitter, ChallengerConfig,
    ChallengerMetrics, Driver, DriverComponents, GameScanner, OutputValidator,
};

/// Top-level challenger service.
#[derive(Debug)]
pub struct ChallengerService;

impl ChallengerService {
    /// Runs the full challenger service lifecycle.
    ///
    /// # Errors
    ///
    /// Returns an error if RPC clients cannot connect or onchain
    /// configuration is invalid.
    pub async fn run(config: ChallengerConfig) -> Result<()> {
        // Install the default rustls CryptoProvider before any TLS connections are created.
        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Challenger starting");

        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        let sender_addr = config.signing.address();
        let l1_rpc_url = config.l1_eth_rpc.clone();
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
            config.signing,
            config.tx_manager,
            chain_id,
            Arc::new(BaseTxMetrics::new("challenger")),
        )
        .await
        .map_err(|e| eyre::eyre!("failed to construct tx manager: {e}"))?;
        let submitter = ChallengeSubmitter::new(tx_manager);

        let read_provider = RootProvider::new_http(l1_rpc_url.clone());
        let factory_client = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory_addr,
            read_provider.clone(),
        );
        info!(
            address = %config.dispute_game_factory_addr,
            "DisputeGameFactory client initialized"
        );

        let verifier_client = AggregateVerifierContractClient::new(read_provider.clone());
        let impl_address = factory_client.game_impls(config.game_type).await?;
        if impl_address == Address::ZERO {
            return Err(eyre::eyre!(
                "no AggregateVerifier implementation registered for game type {}",
                config.game_type
            ));
        }
        // The intervals are not read here: they change at the Cobalt activation block, so
        // every consumer resolves them from the starting block of the game it is handling.
        info!(
            impl_address = %impl_address,
            game_type = config.game_type,
            "Resolved AggregateVerifier implementation"
        );

        let anchor_registry_client = AnchorStateRegistryContractClient::new(
            config.anchor_state_registry_addr,
            read_provider,
        );
        info!(
            address = %config.anchor_state_registry_addr,
            "AnchorStateRegistry client initialized"
        );

        let factory_client: Arc<dyn DisputeGameFactoryClient> = Arc::new(factory_client);
        let verifier_client: Arc<dyn AggregateVerifierClient> = Arc::new(verifier_client);
        let anchor_registry_client: Arc<dyn AnchorStateRegistryClient> =
            Arc::new(anchor_registry_client);

        let l2_client = Arc::new(L2Client::new(L2ClientConfig::new(config.l2_eth_rpc.clone()))?);
        info!(endpoint = %config.l2_eth_rpc, "L2 client initialized");

        let proof_requester_config = ProverServiceClientConfig::new(config.zk_rpc_url.to_string())
            .with_request_timeout(config.zk_request_timeout);
        let proof_requester = Arc::new(ProofRequesterClient::connect(&proof_requester_config)?);
        info!(endpoint = %config.zk_rpc_url, "Prover-service requester client initialized");

        let l1_client = L1Client::new(L1ClientConfig::new(l1_rpc_url.clone()))
            .map_err(|e| eyre::eyre!("failed to create L1 client: {e}"))?;
        let l1_provider: Arc<dyn L1Provider> = Arc::new(l1_client);

        let scanner = GameScanner::new(
            Arc::clone(&factory_client),
            Arc::clone(&verifier_client),
            Arc::clone(&anchor_registry_client),
        );

        let anchor_updater = AnchorUpdater::new(
            Arc::clone(&factory_client),
            Arc::clone(&anchor_registry_client),
            Arc::clone(&l2_client) as Arc<dyn L2Provider>,
            config.anchor_state_registry_addr,
            config.game_type,
        );

        let bond_manager = if !config.bond_claim_addresses.is_empty() {
            Some(BondManager::new(
                BondManagerConfig {
                    claim_addresses: config.bond_claim_addresses,
                    l1_rpc_url,
                    lookback: config.bond_discovery_lookback_games,
                    discovery_interval: config.bond_discovery_interval,
                    metrics_enabled: config.metrics.enabled,
                },
                Arc::clone(&factory_client),
                Arc::clone(&l2_client) as Arc<dyn L2Provider>,
                TokioRuntime::new(),
            ))
        } else {
            info!("bond claiming disabled (no --bond-claim-addresses)");
            None
        };

        let validator = OutputValidator::new(l2_client);

        let ready = Arc::new(AtomicBool::new(false));
        let health_handle = tokio::spawn(HealthServer::serve(
            config.health_addr,
            Arc::clone(&ready),
            cancel.clone(),
        ));

        let driver = Driver::new(DriverComponents {
            scanner,
            validator,
            proof_requester,
            submitter,
            l1_provider,
            verifier_client,
            bond_manager,
            anchor_updater,
            poll_interval: config.poll_interval,
            max_proof_duration: config.max_proof_duration,
            tee_submit_retry_limit: config.tee_submit_retry_limit,
            cancel: cancel.child_token(),
        });

        // Signal readiness immediately after initialization — the driver loop
        // itself is purely operational work that should not gate readiness probes.
        ready.store(true, Ordering::SeqCst);
        info!("service is ready");

        // Drop guard ensures child tasks are cancelled even if the driver panics.
        let cancel_guard = cancel.clone().drop_guard();
        driver.run().await;
        drop(cancel_guard);

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
