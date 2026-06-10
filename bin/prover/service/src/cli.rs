//! CLI definition for the prover-service JSON-RPC binary.

use std::net::SocketAddr;

use base_cli_utils::{LogConfig, RuntimeManager};
use base_prover_service::{
    ProverServiceServer, ServerConfig, StatusPoller, WorkerApiConfig, WorkerQueueConfig,
};
use base_prover_service_db::{DatabaseConfig, ProofRequestRepo};
use base_prover_service_protocol::{ProverRequesterApiServer, ProverWorkerApiServer};
use clap::Parser;
use eyre::eyre;
use jsonrpsee::server::Server;
use tracing::info;

base_cli_utils::define_log_args!("BASE_PROVER_SERVICE");
base_cli_utils::define_metrics_args!("BASE_PROVER_SERVICE", 7302);

/// Prover service binary.
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: ServiceArgs,

    /// Logging arguments.
    #[command(flatten)]
    logging: LogArgs,

    /// Metrics arguments.
    #[command(flatten)]
    metrics: MetricsArgs,
}

/// Prover service for proving Base blocks over JSON-RPC.
#[derive(Parser, Debug)]
struct ServiceArgs {
    #[arg(
        long,
        env = "STATUS_POLLER_INTERVAL_SECS",
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    status_poller_interval_secs: u64,

    #[arg(
        long,
        env = "STUCK_REQUEST_TIMEOUT_MINS",
        default_value_t = 10,
        value_parser = clap::value_parser!(i32).range(1..)
    )]
    stuck_request_timeout_mins: i32,

    #[arg(
        long,
        env = "MAX_PROOF_RETRIES",
        default_value_t = 3,
        value_parser = clap::value_parser!(i32).range(0..)
    )]
    max_proof_retries: i32,

    #[arg(
        long,
        env = "WORKER_DEFAULT_LOCK_DURATION_SECONDS",
        default_value_t = 300,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    worker_default_lock_duration_seconds: u32,

    #[arg(
        long,
        env = "WORKER_MAX_LOCK_DURATION_SECONDS",
        default_value_t = 3600,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    worker_max_lock_duration_seconds: u32,

    #[arg(
        long,
        env = "WORKER_QUEUE_RECLAIM_ATTEMPTS",
        default_value_t = 5,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    worker_queue_reclaim_attempts: u32,

    #[arg(
        long,
        env = "WORKER_QUEUE_REAPER_BATCH_SIZE",
        default_value_t = 100,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    worker_queue_reaper_batch_size: u32,

    #[arg(long, env = "RPC_LISTEN_ADDR", default_value = "0.0.0.0:9000")]
    rpc_listen_addr: SocketAddr,
}

impl Cli {
    /// Run the prover service.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { args, logging, metrics } = self;
        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
            base_prover_service::ProverMetrics::init();
        })?;
        RuntimeManager::new().run_until_ctrl_c(async move { args.run().await })
    }
}

impl ServiceArgs {
    /// Runs the prover service.
    async fn run(self) -> eyre::Result<()> {
        self.validate_config()?;

        info!("initializing database connection");
        let db_config = DatabaseConfig::from_env().map_err(|e| eyre!(e))?;
        let pool = db_config.init_pool().await.map_err(|e| eyre!(e))?;
        let repo = ProofRequestRepo::new(pool);
        info!("database connection initialized");

        let server_config = ServerConfig {
            max_proof_retries: self.max_proof_retries,
            worker: WorkerApiConfig::new(
                self.worker_default_lock_duration_seconds,
                self.worker_max_lock_duration_seconds,
            ),
            worker_queue: WorkerQueueConfig {
                reclaim_attempts: self.worker_queue_reclaim_attempts,
                reaper_batch_size: self.worker_queue_reaper_batch_size,
            },
        };

        info!("starting status poller");
        let status_poller = StatusPoller::new(
            repo.clone(),
            self.status_poller_interval_secs,
            self.stuck_request_timeout_mins,
            self.max_proof_retries,
            server_config.worker_queue,
        );
        let status_handle = tokio::spawn(async move {
            status_poller.run().await;
        });

        let prover_server = ProverServiceServer::new(repo, server_config);

        let mut rpc_module = ProverWorkerApiServer::into_rpc(prover_server.clone());
        rpc_module
            .merge(ProverRequesterApiServer::into_rpc(prover_server))
            .map_err(|e| eyre!("failed to merge requester and worker RPC modules: {e}"))?;

        let rpc_server = Server::builder()
            .build(self.rpc_listen_addr)
            .await
            .map_err(|e| eyre!("failed to bind RPC server: {e}"))?;
        let local_addr =
            rpc_server.local_addr().map_err(|e| eyre!("failed to read RPC server address: {e}"))?;
        info!(addr = %local_addr, "starting prover JSON-RPC service");
        let server_handle = rpc_server.start(rpc_module);

        let result: eyre::Result<()> = tokio::select! {
            res = status_handle => match res {
                Ok(()) => Err(eyre!("status poller exited unexpectedly")),
                Err(e) => Err(eyre!("status poller panicked: {e}")),
            },
            () = server_handle.stopped() => Err(eyre!("RPC server stopped unexpectedly")),
        };

        result
    }

    fn validate_config(&self) -> eyre::Result<()> {
        if self.worker_default_lock_duration_seconds > self.worker_max_lock_duration_seconds {
            eyre::bail!(
                "WORKER_DEFAULT_LOCK_DURATION_SECONDS must be less than or equal to \
                 WORKER_MAX_LOCK_DURATION_SECONDS"
            );
        }

        info!("configuration validated");
        Ok(())
    }
}
