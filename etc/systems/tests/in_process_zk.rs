//! Starts the in-process prover-service and ZK host against a live stack.

#[path = "common/cobalt.rs"]
mod cobalt;
mod common;

use std::time::Duration;

use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_prover_service_protocol::GetProofRequest;
use base_system_tests::{InProcessProverService, InProcessZkHost};
use eyre::{Result, WrapErr, bail, ensure};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Boots Postgres + prover JSON-RPC + a `DryRun` ZK host. Does not execute SP1.
#[tokio::test(flavor = "multi_thread")]
async fn in_process_prover_and_zk_host_start() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    info!("starting Cobalt stack");
    let (system, _provider) = cobalt::start_cobalt_system().await?;

    info!("starting in-process prover-service");
    let service = InProcessProverService::start().await?;
    let client = ProofRequesterClient::connect(
        &ProverServiceClientConfig::new(service.url().as_str())
            .with_request_timeout(Duration::from_secs(30)),
    )
    .wrap_err("failed to connect requester client to in-process prover-service")?;
    let Err(error) = client
        .get_proof(GetProofRequest { session_id: "missing-in-process-zk-smoke".to_owned() })
        .await
    else {
        bail!("get_proof for an unknown session must fail");
    };
    ensure!(error.is_not_found(), "get_proof missing session should be not-found, got {error}");
    info!(url = %service.url(), "prover-service accepted JSON-RPC");

    info!("starting in-process ZK host");
    let _host = InProcessZkHost::start(&system, service.url()).await?;
    info!("in-process ZK host polled the worker API");

    Ok(())
}
