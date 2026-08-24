//! In-process prover-service JSON-RPC server backed by a Postgres testcontainer.

use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use base_prover_service::{
    ProverServiceServer, ServerConfig, StatusPoller, WorkerApiConfig, WorkerQueueConfig,
};
use base_prover_service_db::{DatabaseConfig, ProofRequestRepo};
use base_prover_service_protocol::{ProverRequesterApiServer, ProverWorkerApiServer};
use eyre::{Result, WrapErr, ensure};
use jsonrpsee::server::{Server, ServerHandle};
use sqlx::Executor;
use testcontainers::{ContainerAsync, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::task::JoinHandle;
use tracing::info;
use url::Url;

/// Production default shared with the prover-service binary.
const MAX_PROOF_RETRIES: i32 = 3;
/// Dry-run SP1 execute can outlive the production 300s default lock.
const DEFAULT_LOCK_DURATION_SECONDS: u32 = 1800;
const MAX_LOCK_DURATION_SECONDS: u32 = 3600;
const STATUS_POLLER_INTERVAL_SECS: u64 = 30;
const STUCK_REQUEST_TIMEOUT_MINS: i32 = 10;

/// In-process prover-service requester and worker JSON-RPC on one listen address.
pub struct InProcessProverService {
    /// Shared requester and worker JSON-RPC URL.
    url: Url,
    server_handle: Option<ServerHandle>,
    status_handle: JoinHandle<()>,
    _postgres: ContainerAsync<Postgres>,
}

impl std::fmt::Debug for InProcessProverService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessProverService").field("url", &self.url).finish_non_exhaustive()
    }
}

impl InProcessProverService {
    /// Starts Postgres, applies prover-service migrations, and binds JSON-RPC on `127.0.0.1:0`.
    pub async fn start() -> Result<Self> {
        let postgres = Postgres::default()
            .start()
            .await
            .wrap_err("failed to start prover-service Postgres testcontainer")?;
        let port = postgres
            .get_host_port_ipv4(5432)
            .await
            .wrap_err("failed to map prover-service Postgres port")?;
        let database_url =
            format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres?sslmode=disable");

        let db_config = DatabaseConfig {
            url: database_url,
            max_connections: 5,
            connection_timeout: Duration::from_secs(5),
        };
        let pool = db_config.init_pool().await.map_err(|error| {
            eyre::eyre!("failed to connect to prover-service Postgres: {error}")
        })?;
        apply_prover_migrations(&pool).await?;
        let repo = ProofRequestRepo::new(pool);

        let server_config = ServerConfig {
            max_proof_retries: MAX_PROOF_RETRIES,
            worker: WorkerApiConfig::new(DEFAULT_LOCK_DURATION_SECONDS, MAX_LOCK_DURATION_SECONDS),
            worker_queue: WorkerQueueConfig::default(),
        };
        let status_poller = StatusPoller::new(
            repo.clone(),
            STATUS_POLLER_INTERVAL_SECS,
            STUCK_REQUEST_TIMEOUT_MINS,
            MAX_PROOF_RETRIES,
            server_config.worker_queue,
        );
        let status_handle = tokio::spawn(async move {
            status_poller.run().await;
        });

        let server = ProverServiceServer::new(repo, server_config);
        let mut module = ProverWorkerApiServer::into_rpc(server.clone());
        module
            .merge(ProverRequesterApiServer::into_rpc(server))
            .wrap_err("requester and worker namespaces should not collide")?;

        let addr: SocketAddr =
            "127.0.0.1:0".parse().wrap_err("test listen address should parse")?;
        let rpc_server = Server::builder()
            .build(addr)
            .await
            .wrap_err("failed to bind prover-service JSON-RPC")?;
        let local_addr = rpc_server
            .local_addr()
            .wrap_err("failed to read prover-service JSON-RPC listen address")?;
        let server_handle = rpc_server.start(module);
        let url = Url::parse(&format!("http://{local_addr}"))
            .wrap_err("failed to parse prover-service JSON-RPC URL")?;
        info!(url = %url, "started in-process prover-service");

        Ok(Self { url, server_handle: Some(server_handle), status_handle, _postgres: postgres })
    }

    /// Returns the shared requester and worker JSON-RPC URL.
    pub const fn url(&self) -> &Url {
        &self.url
    }
}

impl Drop for InProcessProverService {
    fn drop(&mut self) {
        self.status_handle.abort();
        if let Some(handle) = self.server_handle.take() {
            let _ = handle.stop();
        }
    }
}

async fn apply_prover_migrations(pool: &sqlx::PgPool) -> Result<()> {
    let migrations_dir = prover_migrations_dir()?;
    let mut files = fs::read_dir(&migrations_dir)
        .wrap_err_with(|| {
            format!("failed to read prover migrations at {}", migrations_dir.display())
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .wrap_err_with(|| {
            format!("failed to list prover migrations in {}", migrations_dir.display())
        })?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect::<Vec<_>>();
    files.sort();
    ensure!(
        !files.is_empty(),
        "no prover-service SQL migrations found in {}",
        migrations_dir.display()
    );

    for path in files {
        let sql = fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read migration {}", path.display()))?;
        pool.execute(sqlx::raw_sql(&sql)).await.wrap_err_with(|| {
            format!("failed to apply prover-service migration {}", path.display())
        })?;
    }
    Ok(())
}

fn prover_migrations_dir() -> Result<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest_dir.join("../../crates/proof/prover-service/db/migrations");
    dir.canonicalize().wrap_err_with(|| {
        format!("prover-service migrations directory not found at {}", dir.display())
    })
}
