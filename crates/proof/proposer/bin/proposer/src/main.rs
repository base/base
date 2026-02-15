//! Proposer binary entry point.
//!
//! Implements the full service lifecycle matching Go's `ProposerService`:
//!
//! 1. Parse and validate configuration
//! 2. Initialise logging and metrics
//! 3. Create RPC clients (L1, L2, rollup, enclave)
//! 4. Create prover, output proposer, and driver
//! 5. Start health / admin HTTP server
//! 6. Start balance monitor (if metrics enabled)
//! 7. Start the driver loop
//! 8. Wait for SIGTERM or SIGINT
//! 9. Graceful shutdown in reverse order

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy_primitives::Address;
use base_proposer::{
    Cli, DriverHandle, ProposerConfig, ProposerDriverControl, SigningConfig,
    contracts::OnchainVerifierContractClient,
    create_output_proposer,
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
///
/// On Unix the handler listens for both signals; on other platforms only
/// SIGINT (ctrl-c) is handled.
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
// Service lifecycle
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // ── 1. Parse CLI and validate configuration ──────────────────────────
    let cli = Cli::parse();
    let config = ProposerConfig::from_cli(cli)?;
    config.log.init_tracing_subscriber()?;

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

    // Create enclave client
    let enclave_client =
        create_enclave_client(config.enclave_rpc.as_str(), config.skip_tls_verify)?;
    info!(endpoint = %config.enclave_rpc, "Enclave client initialized");

    // ── 5. Create contract client, prover, and output proposer ───────────
    let verifier_client = Arc::new(OnchainVerifierContractClient::new(
        config.onchain_verifier_addr,
        config.l1_eth_rpc.clone(),
    )?);
    info!(address = %config.onchain_verifier_addr, "OnchainVerifier client initialized");

    // Create prover
    let prover = Arc::new(Prover::new(
        per_chain_config,
        kona_config.clone(),
        Arc::clone(&l1_client),
        Arc::clone(&l2_client),
        enclave_client,
    ));
    info!(config_hash = ?prover.config_hash(), "Prover initialized");

    let output_proposer = create_output_proposer(
        config.l1_eth_rpc.clone(),
        config.onchain_verifier_addr,
        config.signing.clone(),
        config.retry.clone(),
    )?;
    info!(address = %config.onchain_verifier_addr, "Output proposer initialized");

    // ── 6. Create driver + lifecycle handle ───────────────────────────────
    let driver_config = DriverConfig {
        poll_interval: config.poll_interval,
        min_proposal_interval: config.min_proposal_interval,
        allow_non_finalized: config.allow_non_finalized,
    };
    let driver = Driver::new(
        driver_config,
        prover,
        Arc::clone(&l1_client),
        l2_client,
        rollup_client,
        verifier_client,
        output_proposer,
        cancel.child_token(), // placeholder; DriverHandle replaces on start
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
        tokio::spawn(async move {
            base_proposer::health::serve(addr, ready_flag, admin_driver, health_cancel).await
        })
    };

    // ── 8. Start balance monitor (if metrics enabled) ────────────────────
    let balance_handle: Option<JoinHandle<()>> = if config.metrics.enabled {
        let address = match &config.signing {
            SigningConfig::Local { signer } => signer.address(),
            SigningConfig::Remote { address, .. } => *address,
        };
        let handle = tokio::spawn(balance_monitor(
            Arc::clone(&l1_client),
            address,
            cancel.clone(),
        ));
        info!(%address, "Balance monitor started");
        Some(handle)
    } else {
        None
    };

    // ── 9. Start the driver loop ─────────────────────────────────────────
    driver_handle
        .start_proposer()
        .await
        .map_err(|e| eyre::eyre!(e))?;

    // Mark service as ready for readiness probes.
    ready.store(true, Ordering::SeqCst);
    info!(
        poll_interval = ?config.poll_interval,
        min_proposal_interval = config.min_proposal_interval,
        "Service is ready"
    );

    // ── 10. Wait for shutdown signal ─────────────────────────────────────
    cancel.cancelled().await;
    info!("Shutdown signal received, stopping service...");

    // ── 11. Graceful shutdown (reverse initialisation order) ─────────────
    // Mark as not-ready so readiness probes fail immediately.
    ready.store(false, Ordering::SeqCst);

    // Stop the driver loop.
    if driver_handle.is_running() {
        if let Err(e) = driver_handle.stop_proposer().await {
            warn!(error = e, "Error stopping proposer driver");
        }
    }

    // Wait for balance monitor to finish.
    if let Some(handle) = balance_handle {
        let _ = handle.await;
    }

    // Wait for health server to shut down.
    match health_handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(error = %e, "Health server error during shutdown"),
        Err(e) => warn!(error = %e, "Health server task panicked"),
    }

    info!("Service stopped");
    Ok(())
}
