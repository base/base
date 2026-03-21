//! Full proposer service lifecycle.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use alloy_primitives::Address;
use alloy_provider::Provider;
use base_cli_utils::RuntimeManager;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorStateRegistryContractClient,
    DisputeGameFactoryClient, DisputeGameFactoryContractClient,
};
use base_proof_primitives::ProverClient;
use base_proof_rpc::{
    L1Client, L1ClientConfig, L2Client, L2ClientConfig, RollupClient, RollupClientConfig,
    RollupProvider,
};
use base_tx_manager::{BaseTxMetrics, SimpleTxManager, TxManager};
use eyre::Result;
use jsonrpsee::http_client::HttpClientBuilder;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    balance::balance_monitor,
    config::ProposerConfig,
    driver::{Driver, DriverConfig, DriverHandle, ProposerDriverControl},
    health::serve,
    metrics::record_startup_metrics,
    output_proposer::ProposalSubmitter,
};

/// Runs the full proposer service lifecycle.
///
/// Steps:
/// 1. Initialise logging, TLS, and metrics
/// 2. Create RPC clients (L1, L2, rollup, prover)
/// 3. Read onchain config (`BLOCK_INTERVAL`, `initBond`)
/// 4. Create prover, output proposer, and driver
/// 5. Start health / admin HTTP server
/// 6. Start balance monitor (if metrics enabled)
/// 7. Start the driver loop
/// 8. Wait for SIGTERM or SIGINT
/// 9. Graceful shutdown in reverse order
pub async fn run(config: ProposerConfig) -> Result<()> {
    config.log.init_tracing_subscriber()?;

    // Install the default rustls CryptoProvider before any TLS connections are created.
    // Required by rustls 0.23+ when custom TLS configs are used (e.g. skip_tls_verify).
    let _ = rustls::crypto::ring::default_provider().install_default();

    info!(version = env!("CARGO_PKG_VERSION"), "Proposer starting");

    // ── 1. Global cancellation token and signal handler ──────────────────
    let cancel = CancellationToken::new();
    let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

    // ── 2. Metrics recorder and HTTP server (if enabled) ─────────────────
    config.metrics.init().expect("failed to install Prometheus recorder");

    // Record startup metrics (no-ops if no recorder installed).
    record_startup_metrics(env!("CARGO_PKG_VERSION"));

    // ── 3. Create RPC clients ────────────────────────────────────────────
    let l1_config = L1ClientConfig::new(config.l1_eth_rpc.clone())
        .with_timeout(config.rpc_timeout)
        .with_retry_config(config.retry.clone())
        .with_skip_tls_verify(config.skip_tls_verify)
        .with_metrics_prefix("base_proposer");
    let l1_client = Arc::new(L1Client::new(l1_config)?);
    info!(endpoint = %config.l1_eth_rpc, "L1 client initialized");

    // Create L2 client
    let l2_config = L2ClientConfig::new(config.l2_eth_rpc.clone())
        .with_timeout(config.rpc_timeout)
        .with_retry_config(config.retry.clone())
        .with_skip_tls_verify(config.skip_tls_verify)
        .with_metrics_prefix("base_proposer");
    let l2_client = Arc::new(L2Client::new(l2_config)?);
    info!(endpoint = %config.l2_eth_rpc, "L2 client initialized");

    // Create Rollup client
    let rollup_rpc = config.rollup_rpc.clone();
    let rollup_config = RollupClientConfig::new(rollup_rpc.clone())
        .with_timeout(config.rpc_timeout)
        .with_retry_config(config.retry.clone())
        .with_skip_tls_verify(config.skip_tls_verify);
    let rollup_client = Arc::new(RollupClient::new(rollup_config)?);
    info!(endpoint = %rollup_rpc, "Rollup client initialized");

    // Fetch chain configuration from op-node
    info!("Fetching chain configuration from rollup RPC...");
    let chain_config = rollup_client.rollup_config().await?;
    info!(chain_id = %chain_config.l2_chain_id.id(), "Chain configuration loaded");

    let prover_client = HttpClientBuilder::default()
        .request_timeout(crate::constants::PROVER_TIMEOUT)
        .build(config.prover_rpc.as_str())
        .map_err(|e| eyre::eyre!("failed to create prover RPC client: {e}"))?;
    info!(endpoint = %config.prover_rpc, "Prover RPC client initialized");

    // ── 4. Create contract clients and read onchain config ──────────────
    let anchor_registry = Arc::new(AnchorStateRegistryContractClient::new(
        config.anchor_state_registry_addr,
        config.l1_eth_rpc.clone(),
    )?);
    info!(address = %config.anchor_state_registry_addr, "AnchorStateRegistry client initialized");

    let factory_client = DisputeGameFactoryContractClient::new(
        config.dispute_game_factory_addr,
        config.l1_eth_rpc.clone(),
    )?;
    info!(address = %config.dispute_game_factory_addr, "DisputeGameFactory client initialized");

    // Read BLOCK_INTERVAL from the AggregateVerifier implementation.
    let verifier_client = AggregateVerifierContractClient::new(config.l1_eth_rpc.clone())?;
    let impl_address = factory_client.game_impls(config.game_type).await?;
    if impl_address == Address::ZERO {
        return Err(eyre::eyre!(
            "no AggregateVerifier implementation registered for game type {}",
            config.game_type
        ));
    }
    let block_interval = verifier_client.read_block_interval(impl_address).await?;
    let intermediate_block_interval =
        verifier_client.read_intermediate_block_interval(impl_address).await?;
    if block_interval < 2 {
        return Err(eyre::eyre!(
            "BLOCK_INTERVAL ({block_interval}) must be at least 2; single-block proposals are not supported"
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
        "Read BLOCK_INTERVAL and INTERMEDIATE_BLOCK_INTERVAL from AggregateVerifier"
    );

    let init_bond = factory_client.init_bonds(config.game_type).await?;
    info!(init_bond = %init_bond, game_type = config.game_type, "Read initBond from DisputeGameFactory");

    // Wrap in Arc for shared ownership.
    let factory_client = Arc::new(factory_client);
    let verifier_client: Arc<dyn AggregateVerifierClient> = Arc::new(verifier_client);

    // ── 5a. Create prover ──────────────────────────────────────────────
    let prover_client: Arc<dyn ProverClient> = Arc::new(prover_client);
    info!("Prover initialized");

    // ── 5b. Create output proposer (or dry-run stub) ────────────────────
    let (output_proposer, proposer_address): (Arc<dyn crate::OutputProposer>, Option<Address>) =
        if config.dry_run {
            info!("Dry-run mode enabled — proofs will be sourced but NOT submitted on-chain");
            (Arc::new(crate::DryRunProposer), None)
        } else {
            let signing = config
                .signing
                .ok_or_else(|| eyre::eyre!("signing config required when not in dry-run mode"))?;
            let tx_config = config.tx_manager.ok_or_else(|| {
                eyre::eyre!("tx manager config required when not in dry-run mode")
            })?;

            let l1_tx_provider = alloy_provider::RootProvider::new_http(config.l1_eth_rpc.clone());
            let l1_chain_id = l1_tx_provider
                .get_chain_id()
                .await
                .map_err(|e| eyre::eyre!("failed to fetch L1 chain ID: {e}"))?;
            let tx_manager = SimpleTxManager::new(
                l1_tx_provider,
                signing,
                tx_config,
                l1_chain_id,
                Arc::new(BaseTxMetrics::new("proposer")),
            )
            .await
            .map_err(|e| eyre::eyre!("failed to construct tx manager: {e}"))?;
            let addr = tx_manager.sender_address();
            info!(%addr, "Transaction manager initialized");

            let submitter = ProposalSubmitter::new(
                tx_manager,
                config.dispute_game_factory_addr,
                config.game_type,
                init_bond,
            );
            (Arc::new(submitter), Some(addr))
        };
    info!("Output proposer initialized");

    // ── 6. Create driver ───────────────────────────────────────────────────
    let driver_config = DriverConfig {
        poll_interval: config.poll_interval,
        block_interval,
        intermediate_block_interval,
        init_bond,
        game_type: config.game_type,
        allow_non_finalized: config.allow_non_finalized,
        proposer_address: proposer_address.unwrap_or_default(),
    };
    let driver = Driver::new(
        driver_config,
        prover_client,
        Arc::clone(&l1_client),
        l2_client,
        rollup_client,
        anchor_registry,
        factory_client,
        verifier_client,
        output_proposer,
        cancel.child_token(),
    );

    let driver_handle: Arc<dyn ProposerDriverControl> =
        Arc::new(DriverHandle::new(driver, cancel.clone()));

    // ── 7. Start health / admin HTTP server ──────────────────────────────
    let ready = Arc::new(AtomicBool::new(false));
    let admin_driver = if config.rpc.enable_admin {
        info!("Admin RPC enabled");
        Some(Arc::clone(&driver_handle))
    } else {
        None
    };
    let health_handle: JoinHandle<Result<()>> = {
        let addr = SocketAddr::new(config.rpc.addr, config.rpc.port);
        let ready_flag = Arc::clone(&ready);
        let health_cancel = cancel.clone();
        tokio::spawn(async move { serve(addr, ready_flag, admin_driver, health_cancel).await })
    };

    // ── 8. Start balance monitor (if metrics enabled and not dry-run) ───
    let balance_handle: Option<JoinHandle<()>> = if config.metrics.enabled
        && let Some(addr) = proposer_address
    {
        let handle = tokio::spawn(balance_monitor(Arc::clone(&l1_client), addr, cancel.clone()));
        info!(%addr, "Balance monitor started");
        Some(handle)
    } else {
        None
    };

    // ── 9. Start the driver loop ─────────────────────────────────────────
    driver_handle.start_proposer().await.map_err(|e| eyre::eyre!(e))?;

    ready.store(true, Ordering::SeqCst);
    info!(
        poll_interval = ?config.poll_interval,
        block_interval,
        game_type = config.game_type,
        "Service is ready"
    );

    // ── 10. Wait for shutdown signal ─────────────────────────────────────
    cancel.cancelled().await;
    info!("Shutdown signal received, stopping service...");

    // ── 11. Graceful shutdown (reverse initialisation order) ─────────────
    ready.store(false, Ordering::SeqCst);

    if driver_handle.is_running()
        && let Err(e) = driver_handle.stop_proposer().await
    {
        warn!(error = e, "Error stopping proposer driver");
    }

    if let Some(handle) = balance_handle {
        let _ = handle.await;
    }

    match health_handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(error = %e, "Health server error during shutdown"),
        Err(e) => warn!(error = %e, "Health server task panicked"),
    }

    if let Err(e) = signal_handle.await {
        warn!(error = %e, "Signal handler task panicked");
    }

    info!("Service stopped");
    Ok(())
}
