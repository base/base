//! Full challenger service lifecycle.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use base_balance_monitor::BalanceMonitorLayer;
use base_cli_utils::RuntimeManager;
use base_health::HealthServer;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorStateRegistryClient,
    AnchorStateRegistryContractClient, DisputeGameFactoryClient, DisputeGameFactoryContractClient,
    OptimismPortalContractClient,
};
use base_proof_rpc::{L1Client, L1ClientConfig, L2Client, L2ClientConfig, L2Provider};
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_runtime::TokioRuntime;
use base_tx_manager::{BaseTxMetrics, SendHandle, SimpleTxManager, TxCandidate, TxManager};
use eyre::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    AnchorUpdater, AttestedWithdrawalRelayer, BondManager, BondManagerConfig, ChallengeSubmitter,
    ChallengerConfig, ChallengerMetrics, DisputeComponents, Driver, DriverComponents, GameScanner,
    L1HeadProvider, OutputValidator, attested_withdrawal_signer_client,
};

/// Shares the managed L1 sender between the challenger and withdrawal relay.
#[derive(Debug, Clone)]
struct SharedTxManager<T>(Arc<T>);

impl<T: TxManager> TxManager for SharedTxManager<T> {
    async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
        self.0.send(candidate).await
    }

    async fn send_async(&self, candidate: TxCandidate) -> SendHandle {
        self.0.send_async(candidate).await
    }

    fn sender_address(&self) -> Address {
        self.0.sender_address()
    }
}

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
    /// 4. Create dispute-game-factory and aggregate-verifier clients; read
    ///    onchain block-interval configuration from the registered
    ///    `AggregateVerifier` implementation
    /// 5. Create the anchor-state-registry client, L2 client, and anchor updater
    ///    (the anchor/bond lifecycle runs in both dispute and no-dispute modes)
    /// 6. Build the bond manager
    /// 7. Build the dispute pipeline dependencies (skipped in no-dispute mode)
    /// 8. Start health HTTP server
    /// 9. Assemble and run the driver
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
        let tx_manager = SharedTxManager(Arc::new(tx_manager));
        let submitter = ChallengeSubmitter::new(tx_manager.clone());

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
        let impl_address = factory_client.game_impls(config.game_type).await?;
        if impl_address == Address::ZERO {
            return Err(eyre::eyre!(
                "no AggregateVerifier implementation registered for game type {}",
                config.game_type
            ));
        }
        let (block_interval, intermediate_block_interval) = tokio::try_join!(
            verifier_client.read_block_interval(impl_address),
            verifier_client.read_intermediate_block_interval(impl_address),
        )?;
        if block_interval == 0 || intermediate_block_interval == 0 {
            return Err(eyre::eyre!(
                "BLOCK_INTERVAL ({block_interval}) and INTERMEDIATE_BLOCK_INTERVAL ({intermediate_block_interval}) must be non-zero"
            ));
        }
        if block_interval % intermediate_block_interval != 0 {
            return Err(eyre::eyre!(
                "BLOCK_INTERVAL ({block_interval}) is not divisible by INTERMEDIATE_BLOCK_INTERVAL ({intermediate_block_interval})"
            ));
        }
        info!(
            block_interval,
            intermediate_block_interval,
            intermediate_roots_count = block_interval / intermediate_block_interval,
            impl_address = %impl_address,
            game_type = config.game_type,
            "Read onchain config from AggregateVerifier"
        );

        let anchor_registry_client = AnchorStateRegistryContractClient::new(
            config.anchor_state_registry_addr,
            l1_rpc_url.clone(),
        )?;
        info!(
            address = %config.anchor_state_registry_addr,
            "AnchorStateRegistry client initialized"
        );

        let factory_client: Arc<dyn DisputeGameFactoryClient> = Arc::new(factory_client);
        let verifier_client: Arc<dyn AggregateVerifierClient> = Arc::new(verifier_client);
        let anchor_registry_client: Arc<dyn AnchorStateRegistryClient> =
            Arc::new(anchor_registry_client);

        // ── 5. L2 client and anchor updater ──────────────────────────────────
        let l2_client = Arc::new(L2Client::new(L2ClientConfig::new(config.l2_eth_rpc.clone()))?);
        info!(endpoint = %config.l2_eth_rpc, "L2 client initialized");

        let anchor_updater = AnchorUpdater::new(
            Arc::clone(&factory_client),
            Arc::clone(&anchor_registry_client),
            Arc::clone(&l2_client) as Arc<dyn L2Provider>,
            config.anchor_state_registry_addr,
            config.game_type,
            block_interval,
            intermediate_block_interval,
        );

        // ── 6. Bond manager ─────────────────────────────────────────────────
        // Required in no-dispute mode (enforced by config validation); optional
        // otherwise.
        let bond_manager = if !config.bond_claim_addresses.is_empty() {
            Some(BondManager::new(
                BondManagerConfig {
                    claim_addresses: config.bond_claim_addresses,
                    l1_rpc_url: l1_rpc_url.clone(),
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

        // ── 7. Dispute pipeline dependencies (skipped in no-dispute mode) ────
        // No-dispute mode runs only the bond/anchor lifecycle, so the
        // prover-service client, scanner, validator, and TEE head provider are
        // never constructed. The type annotation pins the `Driver`'s `L2`/`P`
        // generics, which are otherwise unconstrained when `dispute` is `None`.
        let dispute: Option<DisputeComponents<L2Client, ProofRequesterClient>> = if config
            .no_dispute
        {
            info!("no-dispute mode: skipping prover-service, scanner, and validator");
            None
        } else {
            let zk_rpc_url = config.zk_rpc_url.as_ref().expect("zk_rpc_url is Some when disputing");
            let proof_requester_config = ProverServiceClientConfig::new(zk_rpc_url.to_string())
                .with_request_timeout(config.zk_request_timeout);
            let proof_requester = Arc::new(ProofRequesterClient::connect(&proof_requester_config)?);
            info!(endpoint = %zk_rpc_url, "Prover-service requester client initialized");

            let l1_client = L1Client::new(L1ClientConfig::new(l1_rpc_url.clone()))
                .map_err(|e| eyre::eyre!("failed to create TEE L1 client: {e}"))?;
            let tee: Option<Arc<dyn L1HeadProvider>> =
                Some(Arc::new(l1_client) as Arc<dyn L1HeadProvider>);

            let scanner = GameScanner::new(
                Arc::clone(&factory_client),
                Arc::clone(&verifier_client),
                Arc::clone(&anchor_registry_client),
            );
            let validator = OutputValidator::new(Arc::clone(&l2_client));
            Some(DisputeComponents {
                scanner,
                validator,
                proof_requester,
                tee,
                max_proof_duration: config.max_proof_duration,
                tee_submit_retry_limit: config.tee_submit_retry_limit,
            })
        };

        // ── 8. Start the optional attested-withdrawal relay ──────────────────
        let relay_handle = if let Some(relay_config) = config.attested_withdrawal_relay {
            let signer = attested_withdrawal_signer_client(&relay_config.enclave_rpc_url)?;
            let portal =
                OptimismPortalContractClient::new(relay_config.portal_address, l1_rpc_url.clone());
            let relayer = AttestedWithdrawalRelayer::new(
                relay_config,
                Arc::clone(&l2_client),
                signer,
                portal,
                tx_manager.clone(),
            )
            .await?;
            let relay_cancel = cancel.child_token();
            info!("attested withdrawal relay started");
            Some(tokio::spawn(relayer.run(relay_cancel)))
        } else {
            None
        };

        // ── 9. Start health HTTP server ──────────────────────────────────────
        let ready = Arc::new(AtomicBool::new(false));
        let health_handle = tokio::spawn(HealthServer::serve(
            config.health_addr,
            Arc::clone(&ready),
            cancel.clone(),
        ));

        // ── 10. Run driver ───────────────────────────────────────────────────
        let driver = Driver::new(DriverComponents {
            dispute,
            submitter,
            verifier_client,
            bond_manager,
            anchor_updater,
            poll_interval: config.poll_interval,
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

        // ── 11. Graceful shutdown ─────────────────────────────────────────────
        info!("Driver stopped, shutting down...");
        ready.store(false, Ordering::SeqCst);

        if let Some(relay_handle) = relay_handle
            && let Err(error) = relay_handle.await
        {
            warn!(error = %error, "attested withdrawal relay task panicked");
        }

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
