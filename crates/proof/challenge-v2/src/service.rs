//! Top-level challenger service: wires every subsystem and runs them
//! until a shutdown signal.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_provider::{Provider, ProviderBuilder};
use base_balance_monitor::BalanceMonitorLayer;
use base_cli_utils::RuntimeManager;
use base_health::HealthServer;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorStateRegistryClient,
    AnchorStateRegistryContractClient, DisputeGameFactoryClient, DisputeGameFactoryContractClient,
};
use base_proof_primitives::ProverClient;
use base_proof_rpc::{L1Client, L1ClientConfig, L1Provider, L2Client, L2ClientConfig, L2Provider};
use base_tx_manager::{BaseTxMetrics, SimpleTxManager};
use base_zk_client::{ZkProofClient, ZkProofClientConfig, ZkProofProvider};
use eyre::{Result, WrapErr};
use jsonrpsee::http_client::HttpClientBuilder;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    BondDiscovery, BondPool, BondWorkerDeps, ChallengerConfig, DelayedWETHResolver, GameDiscovery,
    GamePool, GameWorkerConfig, GameWorkerDeps, L1DelayedWETHResolver, L2OutputValidator, Metrics,
    OutputValidator, RpcTeeProofProvider, SubmissionTask, TeeProofProvider,
};

/// Top-level challenger service.
#[derive(Debug)]
pub struct ChallengerService;

impl ChallengerService {
    /// Maximum concurrent `Violation::detect` runs across the game pool.
    const DETECT_SEMAPHORE_PERMITS: usize = 32;

    /// Buffer between [`GameDiscovery`] and [`GamePool`].
    const GAME_CHANNEL_CAPACITY: usize = 256;

    /// Buffer between [`BondDiscovery`] and [`BondPool`].
    const BOND_CHANNEL_CAPACITY: usize = 256;

    /// Buffer between every [`crate::SubmissionHandle`] and the single
    /// [`SubmissionTask`].
    const SUBMISSION_CHANNEL_CAPACITY: usize = 256;

    /// Connection timeout for the ZK service gRPC channel.
    const ZK_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Builds every subsystem from `config`, spawns the long-running
    /// tasks, serves the health endpoint, and waits for shutdown.
    ///
    /// Phases:
    ///
    /// 1. Process-wide bootstrap (TLS, cancellation, signal handler).
    /// 2. Transaction manager (L1 chain id read + signer wallet).
    /// 3. On-chain reader clients (factory, verifier, anchor registry).
    /// 4. Chain RPC clients (L1, L2) shared across subsystems.
    /// 5. Proving services (output validator, ZK, TEE) and bond resolver.
    /// 6. Worker dependency bundles ([`GameWorkerDeps`], [`BondWorkerDeps`]).
    /// 7. Submission task and pipeline channels.
    /// 8. Discoveries and pools wired to the channels.
    /// 9. Long-running task spawn (game + bond pipelines + submission).
    /// 10. Health server and readiness flag.
    /// 11. Wait for shutdown.
    /// 12. Graceful join of every spawned task.
    pub async fn run(config: ChallengerConfig) -> Result<()> {
        // 1. Process-wide bootstrap.
        let _ = rustls::crypto::ring::default_provider().install_default();
        info!(version = env!("CARGO_PKG_VERSION"), "Challenger v2 starting");
        Metrics::record_startup();
        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        // 2. Transaction manager.
        let sender_address = config.signer_config.address();
        let l1_tx_provider = if config.metrics.enabled {
            let layer = Self::start_balance_monitor(sender_address, cancel.clone());
            ProviderBuilder::new().layer(layer).connect_http(config.l1_eth_rpc.clone())
        } else {
            ProviderBuilder::new().connect_http(config.l1_eth_rpc.clone())
        };
        let chain_id =
            l1_tx_provider.get_chain_id().await.wrap_err("failed to fetch L1 chain ID")?;
        let tx_manager = SimpleTxManager::new(
            l1_tx_provider,
            config.signer_config,
            config.tx_manager_config,
            chain_id,
            Arc::new(BaseTxMetrics::new("challenger_v2")),
        )
        .await
        .wrap_err("failed to construct tx manager")?;
        info!(addr = %sender_address, "Transaction manager initialized");

        // 3. On-chain reader clients.
        let factory: Arc<dyn DisputeGameFactoryClient> =
            Arc::new(DisputeGameFactoryContractClient::new(
                config.dispute_game_factory_addr,
                config.l1_eth_rpc.clone(),
            )?);
        let verifier: Arc<dyn AggregateVerifierClient> =
            Arc::new(AggregateVerifierContractClient::new(config.l1_eth_rpc.clone())?);
        let anchor_registry: Arc<dyn AnchorStateRegistryClient> =
            Arc::new(AnchorStateRegistryContractClient::new(
                config.anchor_state_registry_addr,
                config.l1_eth_rpc.clone(),
            )?);

        // 4. Chain RPC clients (shared).
        let l1_client: Arc<dyn L1Provider> =
            Arc::new(L1Client::new(L1ClientConfig::new(config.l1_eth_rpc.clone()))?);
        let l2_client: Arc<dyn L2Provider> =
            Arc::new(L2Client::new(L2ClientConfig::new(config.l2_eth_rpc.clone()))?);

        // 5. Proving services and bond resolver.
        let validator: Arc<dyn OutputValidator> =
            Arc::new(L2OutputValidator::new(Arc::clone(&l2_client)));
        let zk_prover: Arc<dyn ZkProofProvider> =
            Arc::new(ZkProofClient::new(&ZkProofClientConfig {
                endpoint: config.zk_rpc_url.clone(),
                connect_timeout: Self::ZK_CONNECT_TIMEOUT,
                request_timeout: config.zk_request_timeout,
            })?);
        let tee_prover_client: Arc<dyn ProverClient> = Arc::new(
            HttpClientBuilder::default()
                .request_timeout(config.tee_request_timeout)
                .build(config.tee_rpc_url.as_str())
                .wrap_err("failed to create TEE RPC client")?,
        );
        let tee_prover: Arc<dyn TeeProofProvider> = Arc::new(RpcTeeProofProvider::new(
            tee_prover_client,
            Arc::clone(&l1_client),
            Arc::clone(&l2_client),
            sender_address,
        ));
        let delayed_weth_resolver: Arc<dyn DelayedWETHResolver> =
            Arc::new(L1DelayedWETHResolver::new(Arc::clone(&verifier), config.l1_eth_rpc.clone()));

        // 6. Worker dependency bundles.
        let game_deps = Arc::new(GameWorkerDeps::new(
            validator,
            Arc::clone(&verifier),
            zk_prover,
            tee_prover,
            Arc::new(Semaphore::new(Self::DETECT_SEMAPHORE_PERMITS)),
            GameWorkerConfig {
                sender_address,
                max_proof_retries: config.max_proof_retries,
                proof_poll_interval: config.proof_poll_interval,
                max_proof_duration: config.max_proof_duration,
            },
        ));
        let (submission_task, submission_handle) =
            SubmissionTask::new(tx_manager, Self::SUBMISSION_CHANNEL_CAPACITY);
        let bond_deps = Arc::new(BondWorkerDeps::new(
            Arc::clone(&verifier),
            delayed_weth_resolver,
            submission_handle.clone(),
            config.bond_claim_addresses.iter().copied().collect(),
        ));

        // 7. Pipeline channels.
        let (game_tx, game_rx) = mpsc::channel(Self::GAME_CHANNEL_CAPACITY);
        let (bond_tx, bond_rx) = mpsc::channel(Self::BOND_CHANNEL_CAPACITY);

        // 8. Discoveries and pools.
        let game_discovery = GameDiscovery::new(
            Arc::clone(&factory),
            Arc::clone(&verifier),
            anchor_registry,
            config.game_type,
        );
        let game_pool = GamePool::new(Arc::clone(&game_deps), submission_handle);
        let bond_discovery = BondDiscovery::new(
            Arc::clone(&factory),
            Arc::clone(&verifier),
            config.bond_claim_addresses.iter().copied().collect(),
            config.bond_discovery_max_age,
            config.bond_discovery_interval,
        );
        let bond_pool = BondPool::new(Arc::clone(&bond_deps));

        // 9. Spawn long-running tasks.
        let game_discovery_handle =
            tokio::spawn(game_discovery.run(game_tx, config.game_poll_interval, cancel.clone()));
        let game_pool_handle = tokio::spawn(game_pool.run(game_rx, cancel.clone()));
        let submission_task_handle = tokio::spawn(submission_task.run(cancel.clone()));
        let bond_discovery_handle = tokio::spawn(bond_discovery.run(bond_tx, cancel.clone()));
        let bond_pool_handle = tokio::spawn(bond_pool.run(bond_rx, cancel.clone()));

        // 10. Health server and readiness flag.
        let ready = Arc::new(AtomicBool::new(false));
        let health_handle = {
            let addr = config.health_addr;
            let ready = Arc::clone(&ready);
            let cancel = cancel.clone();
            tokio::spawn(async move { HealthServer::serve(addr, ready, cancel).await })
        };
        ready.store(true, Ordering::SeqCst);
        info!(
            game_poll_interval = ?config.game_poll_interval,
            bond_discovery_interval = ?config.bond_discovery_interval,
            game_type = config.game_type,
            "Service is ready"
        );

        // 11. Wait for shutdown.
        cancel.cancelled().await;
        info!("Shutdown signal received, stopping service");
        ready.store(false, Ordering::SeqCst);

        // 12. Graceful join.
        for (name, handle) in [
            ("game_discovery", game_discovery_handle),
            ("game_pool", game_pool_handle),
            ("submission_task", submission_task_handle),
            ("bond_discovery", bond_discovery_handle),
            ("bond_pool", bond_pool_handle),
        ] {
            if let Err(e) = handle.await {
                warn!(task = name, error = %e, "task did not exit cleanly");
            }
        }
        match health_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "health server error during shutdown"),
            Err(e) => warn!(error = %e, "health server task panicked"),
        }
        signal_handle.abort();
        match signal_handle.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => warn!(error = %e, "signal handler task panicked"),
        }

        info!("Service stopped");
        Ok(())
    }

    /// Builds a [`BalanceMonitorLayer`] for `sender_address` and spawns
    /// a forwarder that pushes each new balance into the
    /// `account_balance_wei` gauge.
    fn start_balance_monitor(
        sender_address: alloy_primitives::Address,
        cancel: CancellationToken,
    ) -> BalanceMonitorLayer {
        let (layer, mut balance_rx) = BalanceMonitorLayer::new(
            sender_address,
            cancel,
            BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
        );

        tokio::spawn(async move {
            while balance_rx.changed().await.is_ok() {
                Metrics::account_balance_wei().set(f64::from(*balance_rx.borrow_and_update()));
            }
        });

        info!(addr = %sender_address, "Balance monitor started");
        layer
    }
}
