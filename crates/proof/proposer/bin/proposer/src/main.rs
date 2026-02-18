//! Proposer binary entry point.
//!
//! Implements the full service lifecycle matching Go's `ProposerService`:
//!
//! 1. Parse and validate configuration
//! 2. Initialise logging and metrics
//! 3. Create RPC clients (L1, L2, rollup, enclave)
//! 4. Read on-chain config (BLOCK_INTERVAL, initBond)
//! 5. Create prover, output proposer, and driver
//! 6. Recover parent game state from on-chain data
//! 7. Start health / admin HTTP server
//! 8. Start balance monitor (if metrics enabled)
//! 9. Start the driver loop
//! 10. Wait for SIGTERM or SIGINT
//! 11. Graceful shutdown in reverse order

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy_primitives::{Address, B256};
use base_proposer::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorStateRegistryContractClient,
    Cli, DisputeGameFactoryClient, DisputeGameFactoryContractClient, DriverHandle, ProposerConfig,
    ProposerDriverControl, SigningConfig, create_output_proposer,
    driver::{Driver, DriverConfig},
    enclave::{create_enclave_client, rollup_config_to_per_chain_config},
    prover::Prover,
    rpc::{
        L1Client, L1ClientConfig, L1ClientImpl, L2ClientConfig, RollupClient, RollupClientConfig,
        RollupClientImpl, create_l2_client,
    },
};
use clap::Parser;
use eyre::Result;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Balance monitor
// ---------------------------------------------------------------------------

/// Balance polling interval.
const BALANCE_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Periodically polls the L1 balance of `address` and records it as a Prometheus gauge.
async fn balance_monitor<L1: L1Client>(
    l1_client: Arc<L1>,
    address: Address,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(BALANCE_POLL_INTERVAL) => {
                match l1_client.get_balance(address).await {
                    Ok(balance) => {
                        // U256 -> f64 conversion: safe enough for gauge display.
                        let balance_f64: f64 = balance.to_string().parse().unwrap_or(f64::MAX);
                        metrics::gauge!(base_proposer::metrics::ACCOUNT_BALANCE_WEI).set(balance_f64);
                    }
                    Err(e) => {
                        debug!(error = %e, "Failed to fetch account balance");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

/// Installs SIGTERM + SIGINT handlers that cancel the given token.
fn setup_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result.expect("failed to listen for SIGINT");
                    info!("Received SIGINT");
                }
                _ = sigterm.recv() => {
                    info!("Received SIGTERM");
                }
            }
        }

        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen for SIGINT");
            info!("Received SIGINT");
        }

        cancel.cancel();
    });
}

// ---------------------------------------------------------------------------
// Parent game state recovery
// ---------------------------------------------------------------------------

/// Recovers parent game state from on-chain data on startup.
///
/// Walks backwards through the `DisputeGameFactory` to find the most recent
/// game of the correct `game_type`. Returns `(game_index, output_root, l2_block_number)`
/// if found, or `None` if no matching game exists.
async fn recover_parent_game_state_standalone(
    factory: &DisputeGameFactoryContractClient,
    verifier: &AggregateVerifierContractClient,
    game_type: u32,
) -> Result<Option<(u32, alloy_primitives::B256, u64)>> {
    let count = factory.game_count().await?;
    if count == 0 {
        info!("No existing games found, will start from anchor registry");
        return Ok(None);
    }

    let search_count = count.min(base_proposer::MAX_GAME_RECOVERY_LOOKBACK);

    for i in 0..search_count {
        let game_index = count - 1 - i;
        let game = factory.game_at_index(game_index).await?;

        if game.game_type != game_type {
            continue;
        }

        let game_info = verifier.game_info(game.proxy).await?;

        let idx: u32 = game_index
            .try_into()
            .map_err(|_| eyre::eyre!("game index {game_index} exceeds u32"))?;

        info!(
            game_index,
            game_proxy = %game.proxy,
            output_root = ?game_info.root_claim,
            l2_block_number = game_info.l2_block_number,
            "Recovered parent game state from on-chain"
        );
        return Ok(Some((idx, game_info.root_claim, game_info.l2_block_number)));
    }

    info!(
        game_type,
        searched = search_count,
        "No games found for our game type in recent history, will start from anchor registry"
    );
    Ok(None)
}

// ---------------------------------------------------------------------------
// Service lifecycle
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // ── 1. Parse CLI and validate configuration ──────────────────────────
    let cli = Cli::parse();
    let config = ProposerConfig::from_cli(cli)?;
    config.log.init_tracing_subscriber()?;

    // Install the default rustls CryptoProvider before any TLS connections are created.
    // Required by rustls 0.23+ when custom TLS configs are used (e.g. skip_tls_verify).
    let _ = rustls::crypto::ring::default_provider().install_default();

    info!(version = env!("CARGO_PKG_VERSION"), "Proposer starting");

    // ── 2. Global cancellation token and signal handler ──────────────────
    let cancel = CancellationToken::new();
    setup_signal_handler(cancel.clone());

    // ── 3. Metrics recorder and HTTP server (if enabled) ─────────────────
    if config.metrics.enabled {
        let addr = SocketAddr::new(config.metrics.addr, config.metrics.port);
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()
            .expect("failed to install Prometheus recorder");
        info!(%addr, "Metrics server started");
    }

    // Record startup metrics (no-ops if no recorder installed).
    base_proposer::metrics::record_startup_metrics(env!("CARGO_PKG_VERSION"));

    // ── 4. Create RPC clients ────────────────────────────────────────────
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
    let kona_config = rollup_client.rollup_config().await?;
    let per_chain_config = rollup_config_to_per_chain_config(&kona_config)?;
    info!(chain_id = %per_chain_config.chain_id, "Chain configuration loaded");

    let enclave_client =
        create_enclave_client(config.enclave_rpc.as_str(), config.skip_tls_verify)?;
    info!(endpoint = %config.enclave_rpc, "Enclave client initialized");

    // ── 5. Create contract clients and read on-chain config ──────────────
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
    info!(
        block_interval,
        impl_address = %impl_address,
        game_type = config.game_type,
        "Read BLOCK_INTERVAL from AggregateVerifier"
    );

    let init_bond = factory_client.init_bonds(config.game_type).await?;
    info!(init_bond = %init_bond, game_type = config.game_type, "Read initBond from DisputeGameFactory");

    // Wrap in Arc for shared ownership.
    let factory_client = Arc::new(factory_client);

    // ── 6. Create prover and output proposer ─────────────────────────────
    let proposer_address = match &config.signing {
        SigningConfig::Local { signer } => signer.address(),
        SigningConfig::Remote { address, .. } => *address,
    };

    let prover = Arc::new(Prover::new(
        per_chain_config,
        kona_config.clone(),
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

    // ── 7. Recover parent game state before creating driver ─────────────
    // Recovery needs direct access to the factory client before it's moved
    // into the driver. We store the recovered state and set it after driver creation.
    let recovered_state: Option<(u32, B256, u64)> = match recover_parent_game_state_standalone(
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

    // ── 8. Create driver + lifecycle handle ───────────────────────────────
    let driver_config = DriverConfig {
        poll_interval: config.poll_interval,
        block_interval,
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
        tokio::spawn(async move {
            base_proposer::health::serve(addr, ready_flag, admin_driver, health_cancel).await
        })
    };

    // ── 9. Start balance monitor (if metrics enabled) ────────────────────
    let balance_handle: Option<JoinHandle<()>> = if config.metrics.enabled {
        let handle = tokio::spawn(balance_monitor(
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
    driver_handle
        .start_proposer()
        .await
        .map_err(|e| eyre::eyre!(e))?;

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

    if driver_handle.is_running() {
        if let Err(e) = driver_handle.stop_proposer().await {
            warn!(error = e, "Error stopping proposer driver");
        }
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
