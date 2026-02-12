//! Proposer binary entry point.

use std::sync::Arc;

use base_proposer::{
    Cli, ProposerConfig,
    contracts::OnchainVerifierContractClient,
    driver::{Driver, DriverConfig},
    enclave::{create_enclave_client, rollup_config_to_per_chain_config},
    prover::Prover,
    rpc::{
        L1ClientConfig, L1ClientImpl, L2ClientConfig, RollupClient, RollupClientConfig,
        RollupClientImpl, create_l2_client,
    },
};
use clap::Parser;
use eyre::Result;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = ProposerConfig::from_cli(cli)?;
    config.log.init_tracing_subscriber()?;

    info!(version = env!("CARGO_PKG_VERSION"), "Proposer starting");

    let rollup_rpc = config.rollup_rpc.clone();

    // Create L1 client
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
    info!(
        chain_id = %per_chain_config.chain_id,
        "Chain configuration loaded"
    );

    // Create enclave client
    let enclave_client =
        create_enclave_client(config.enclave_rpc.as_str(), config.skip_tls_verify)?;
    info!(endpoint = %config.enclave_rpc, "Enclave client initialized");

    // Create OnchainVerifier contract client
    let verifier_client = Arc::new(OnchainVerifierContractClient::new(
        config.onchain_verifier_addr,
        config.l1_eth_rpc.clone(),
    )?);
    info!(
        address = %config.onchain_verifier_addr,
        "OnchainVerifier client initialized"
    );

    // Create prover
    let prover = Arc::new(Prover::new(
        per_chain_config,
        kona_config.clone(),
        Arc::clone(&l1_client),
        Arc::clone(&l2_client),
        enclave_client,
    ));
    info!(config_hash = ?prover.config_hash(), "Prover initialized");

    // Create cancellation token and signal handler
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl_c");
        info!("Received shutdown signal");
        cancel_clone.cancel();
    });

    // Create and run driver
    let driver_config = DriverConfig {
        poll_interval: config.poll_interval,
        min_proposal_interval: config.min_proposal_interval,
        allow_non_finalized: config.allow_non_finalized,
    };
    let mut driver = Driver::new(
        driver_config,
        prover,
        l1_client,
        l2_client,
        rollup_client,
        verifier_client,
        cancel,
    );

    info!(
        poll_interval = ?config.poll_interval,
        min_proposal_interval = config.min_proposal_interval,
        "Starting driver loop"
    );

    driver.run().await?;

    Ok(())
}
