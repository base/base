//! Full proposer service lifecycle.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use alloy_primitives::{Address, B256};
use eyre::Result;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    AggregateVerifierContractClient, AnchorStateRegistryContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, Driver, DriverConfig, DriverHandle, L1ClientConfig,
    L1ClientImpl, L2ClientConfig, ProposerConfig, ProposerDriverControl, Prover, RollupClient,
    RollupClientConfig, RollupClientImpl, SigningConfig, create_enclave_client, create_l2_client,
    create_output_proposer, rollup_config_to_per_chain_config,
};

/// Runs the full proposer service lifecycle.
///
/// Steps:
/// 1. Initialise logging, TLS, and metrics
/// 2. Create RPC clients (L1, L2, rollup, enclave)
/// 3. Read onchain config (`BLOCK_INTERVAL`, `initBond`)
/// 4. Create prover, output proposer, and driver
/// 5. Recover parent game state from onchain data
/// 6. Start health / admin HTTP server
/// 7. Start balance monitor (if metrics enabled)
/// 8. Start the driver loop
/// 9. Wait for SIGTERM or SIGINT
/// 10. Graceful shutdown in reverse order
pub async fn run(config: ProposerConfig) -> Result<()> {
    config.log.init_tracing_subscriber()?;

    // Install the default rustls CryptoProvider before any TLS connections are created.
    // Required by rustls 0.23+ when custom TLS configs are used (e.g. skip_tls_verify).
    let _ = rustls::crypto::ring::default_provider().install_default();

    info!(version = env!("CARGO_PKG_VERSION"), "Proposer starting");

    // ── 1. Global cancellation token and signal handler ──────────────────
    let cancel = CancellationToken::new();
    crate::setup_signal_handler(cancel.clone());

    // ── 2. Metrics recorder and HTTP server (if enabled) ─────────────────
    if config.metrics.enabled {
        let addr = SocketAddr::new(config.metrics.addr, config.metrics.port);
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()
            .expect("failed to install Prometheus recorder");
        info!(%addr, "Metrics server started");
    }

    // Record startup metrics (no-ops if no recorder installed).
    crate::record_startup_metrics(env!("CARGO_PKG_VERSION"));

    // ── 3. Create RPC clients ────────────────────────────────────────────
    let l1_config = L1ClientConfig::new(config.l1_eth_rpc.clone())
        .with_timeout(config.rpc_timeout)
        .with_retry_config(config.retry.clone())
        .with_skip_tls_verify(config.skip_tls_verify);
    let l1_client = Arc::new(L1ClientImpl::new(l1_config)?);
    info!(endpoint = %config.l1_eth_rpc, "L1 client initialized");

    // Create L2 client
    let l2_config = L2ClientConfig::new(config.l2_eth_rpc.clone())
        .with_timeout(config.rpc_timeout)
        .with_retry_config(config.retry.clone())
        .with_skip_tls_verify(config.skip_tls_verify);
    let l2_client = Arc::new(create_l2_client(l2_config, config.l2_reth)?);
    info!(endpoint = %config.l2_eth_rpc, reth = config.l2_reth, "L2 client initialized");

    // Create Rollup client
    let rollup_rpc = config.rollup_rpc.clone();
    let rollup_config = RollupClientConfig::new(rollup_rpc.clone())
        .with_timeout(config.rpc_timeout)
        .with_retry_config(config.retry.clone())
        .with_skip_tls_verify(config.skip_tls_verify);
    let rollup_client = Arc::new(RollupClientImpl::new(rollup_config)?);
    info!(endpoint = %rollup_rpc, "Rollup client initialized");

    // Fetch chain configuration from op-node
    info!("Fetching chain configuration from rollup RPC...");
    let chain_config = rollup_client.rollup_config().await?;
    let per_chain_config = rollup_config_to_per_chain_config(&chain_config)?;
    info!(chain_id = %per_chain_config.chain_id, "Chain configuration loaded");

    let enclave_client =
        create_enclave_client(config.enclave_rpc.as_str(), config.skip_tls_verify)?;
    info!(endpoint = %config.enclave_rpc, "Enclave client initialized");

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

    // ── 5. Create prover and output proposer ─────────────────────────────
    let proposer_address = match &config.signing {
        SigningConfig::Local { signer } => signer.address(),
        SigningConfig::Remote { address, .. } => *address,
    };

    let prover = Arc::new(Prover::new(
        per_chain_config,
        chain_config.clone(),
        Arc::clone(&l1_client),
        Arc::clone(&l2_client),
        enclave_client,
        proposer_address,
        config.tee_image_hash,
    ));
    info!(config_hash = ?prover.config_hash(), proposer = %proposer_address, "Prover initialized");

    let output_proposer = create_output_proposer(
        config.l1_eth_rpc.clone(),
        config.dispute_game_factory_addr,
        config.game_type,
        init_bond,
        config.signing.clone(),
        config.retry.clone(),
    )?;
    info!("Output proposer initialized");

    // ── 6. Recover parent game state before creating driver ─────────────
    // Recovery needs direct access to the factory client before it's moved
    // into the driver. We store the recovered state and set it after driver creation.
    let recovered_state: Option<(u32, B256, u64)> =
        match crate::recover_parent_game_state_standalone(
            &factory_client,
            &verifier_client,
            config.game_type,
        )
        .await
        {
            Ok(Some(state)) => Some(state),
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, "Failed to recover parent game state, will start from anchor");
                None
            }
        };

    // ── 7. Create driver + lifecycle handle ───────────────────────────────
    let driver_config = DriverConfig {
        poll_interval: config.poll_interval,
        block_interval,
        intermediate_block_interval,
        init_bond,
        game_type: config.game_type,
        allow_non_finalized: config.allow_non_finalized,
    };
    let mut driver = Driver::new(
        driver_config,
        prover,
        Arc::clone(&l1_client),
        l2_client,
        rollup_client,
        anchor_registry,
        factory_client,
        output_proposer,
        cancel.child_token(),
    );

    // Apply recovered parent game state.
    if let Some((game_index, output_root, l2_block_number)) = recovered_state {
        driver.set_parent_game_state(game_index, output_root, l2_block_number);
    }

    let driver_handle: Arc<dyn ProposerDriverControl> =
        Arc::new(DriverHandle::new(driver, cancel.clone()));

    // ── 8. Start health / admin HTTP server ──────────────────────────────
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
        tokio::spawn(
            async move { crate::serve(addr, ready_flag, admin_driver, health_cancel).await },
        )
    };

    // ── 9. Start balance monitor (if metrics enabled) ────────────────────
    let balance_handle: Option<JoinHandle<()>> = if config.metrics.enabled {
        let handle = tokio::spawn(crate::balance_monitor(
            Arc::clone(&l1_client),
            proposer_address,
            cancel.clone(),
        ));
        info!(%proposer_address, "Balance monitor started");
        Some(handle)
    } else {
        None
    };

    // ── 10. Start the driver loop ────────────────────────────────────────
    driver_handle.start_proposer().await.map_err(|e| eyre::eyre!(e))?;

    ready.store(true, Ordering::SeqCst);
    info!(
        poll_interval = ?config.poll_interval,
        block_interval,
        game_type = config.game_type,
        "Service is ready"
    );

    // ── 11. Wait for shutdown signal ─────────────────────────────────────
    cancel.cancelled().await;
    info!("Shutdown signal received, stopping service...");

    // ── 12. Graceful shutdown (reverse initialisation order) ─────────────
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

    info!("Service stopped");
    Ok(())
}
