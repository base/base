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
use jsonrpsee::server::{Server, ServerConfig as JsonRpcServerConfig};
use tracing::info;

base_cli_utils::define_log_args!("BASE_PROVER_SERVICE");
base_cli_utils::define_metrics_args!("BASE_PROVER_SERVICE", 7302);

const DEFAULT_RPC_MAX_BODY_BYTES: u32 = 32 * 1024 * 1024;

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

    #[arg(long = "rpc-listen-addr", env = "RPC_LISTEN_ADDR", default_value = "0.0.0.0:9000")]
    requester_rpc_listen_addr: SocketAddr,

    #[arg(long, env = "WORKER_RPC_LISTEN_ADDR", default_value = "127.0.0.1:9001")]
    worker_rpc_listen_addr: SocketAddr,

    #[arg(
        long,
        env = "RPC_MAX_REQUEST_BODY_BYTES",
        default_value_t = DEFAULT_RPC_MAX_BODY_BYTES,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    rpc_max_request_body_bytes: u32,

    #[arg(
        long,
        env = "RPC_MAX_RESPONSE_BODY_BYTES",
        default_value_t = DEFAULT_RPC_MAX_BODY_BYTES,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    rpc_max_response_body_bytes: u32,
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
        let mut status_handle = tokio::spawn(async move {
            status_poller.run().await;
        });

        let prover_server = ProverServiceServer::new(repo, server_config);

        let json_rpc_config = JsonRpcServerConfig::builder()
            .max_request_body_size(self.rpc_max_request_body_bytes)
            .max_response_body_size(self.rpc_max_response_body_bytes)
            .build();

        let requester_rpc_module = ProverRequesterApiServer::into_rpc(prover_server.clone());
        let requester_rpc_server = Server::builder()
            .set_config(json_rpc_config.clone())
            .build(self.requester_rpc_listen_addr)
            .await
            .map_err(|e| eyre!("failed to bind requester RPC server: {e}"))?;
        let requester_local_addr = requester_rpc_server
            .local_addr()
            .map_err(|e| eyre!("failed to read requester RPC server address: {e}"))?;
        info!(
            addr = %requester_local_addr,
            max_request_body_bytes = self.rpc_max_request_body_bytes,
            max_response_body_bytes = self.rpc_max_response_body_bytes,
            "starting prover requester JSON-RPC service"
        );
        let requester_server_handle = requester_rpc_server.start(requester_rpc_module);

        let worker_rpc_module = ProverWorkerApiServer::into_rpc(prover_server);
        let worker_rpc_server = Server::builder()
            .set_config(json_rpc_config)
            .build(self.worker_rpc_listen_addr)
            .await
            .map_err(|e| eyre!("failed to bind worker RPC server: {e}"))?;
        let worker_local_addr = worker_rpc_server
            .local_addr()
            .map_err(|e| eyre!("failed to read worker RPC server address: {e}"))?;
        info!(
            addr = %worker_local_addr,
            max_request_body_bytes = self.rpc_max_request_body_bytes,
            max_response_body_bytes = self.rpc_max_response_body_bytes,
            "starting prover worker JSON-RPC service"
        );
        let worker_server_handle = worker_rpc_server.start(worker_rpc_module);

        let result: eyre::Result<()> = tokio::select! {
            res = &mut status_handle => match res {
                Ok(()) => Err(eyre!("status poller exited unexpectedly")),
                Err(e) => Err(eyre!("status poller panicked: {e}")),
            },
            () = requester_server_handle.stopped() => {
                Err(eyre!("requester RPC server stopped unexpectedly"))
            },
            () = worker_server_handle.stopped() => {
                Err(eyre!("worker RPC server stopped unexpectedly"))
            },
        };

        status_handle.abort();

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
