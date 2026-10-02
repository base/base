//! Native main-container migration supervisor with terminal hosting and verified stop.

use std::{
    fmt,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use base_cli_utils::RuntimeManager;
use chrono::Utc;
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_process::Collector;
use sqlx::{Connection, PgConnection};
use tokio::{
    net::TcpListener,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, sleep, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    AuditMigration, Metrics, MigrationError, MigrationHttp, MigrationPhase, MigrationReporter,
    MigrationSession, MigrationState, MigrationStatus, MigrationStore,
};

/// Native managed mode configuration. Intentionally has no secret-bearing Debug implementation.
#[derive(Clone)]
pub struct ManagedMigrationConfig {
    /// Existing migration-owner connection URL, supplied through environment.
    pub database_url: String,
    /// Combined probes/status/metrics listener.
    pub address: SocketAddr,
    /// Whether to install a Prometheus recorder (HTTP probes always enabled).
    pub metrics_enabled: bool,
    /// Process collection interval in seconds.
    pub metrics_interval_secs: u64,
    /// Bounded, nonsensitive same-pod attempt identity.
    pub run_id: String,
    /// Writable state file on the same-pod volume.
    pub state_path: PathBuf,
    /// Absolute cancellation/verification/join budget, normally 30 seconds.
    pub shutdown_timeout: Duration,
}

impl fmt::Debug for ManagedMigrationConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ManagedMigrationConfig { connection and paths redacted }")
    }
}

impl ManagedMigrationConfig {
    /// Validates configuration before binding or starting database work.
    pub fn validate(&self) -> Result<(), MigrationError> {
        if self.run_id.is_empty()
            || self.run_id.len() > 128
            || !self.run_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
            || self.shutdown_timeout.is_zero()
            || self.shutdown_timeout > Duration::from_secs(30)
            || self.metrics_interval_secs == 0
            || self.state_path.parent().is_none()
        {
            return Err(MigrationError::Configuration);
        }
        Ok(())
    }
}

/// Supervises exactly one attempt, then hosts its terminal outcome until signal.
#[derive(Debug, Clone, Copy)]
pub struct ManagedMigration;

impl ManagedMigration {
    /// Entry point for PID1, installing the existing SIGTERM/SIGINT facility.
    pub async fn run(config: ManagedMigrationConfig) -> Result<(), MigrationError> {
        let cancel = CancellationToken::new();
        let signals = RuntimeManager::install_signal_handler(cancel.clone());
        let result = Self::run_until(config, cancel).await;
        signals.abort();
        result
    }

    /// Runs with an externally supplied cancellation request for integration tests.
    pub async fn run_until(
        config: ManagedMigrationConfig,
        cancel: CancellationToken,
    ) -> Result<(), MigrationError> {
        config.validate()?;
        let store = MigrationStore { path: config.state_path.clone() };
        let restored = store.load(&config.run_id)?;
        let mut progress = MigrationReporter::new(config.run_id.clone());
        progress.cancel = cancel.clone();
        progress.store = Some(store);
        if let Some(record) = &restored {
            *progress.status.lock().map_err(|_| MigrationError::Worker)? = record.status.clone();
            *progress.backend.lock().map_err(|_| MigrationError::Worker)? = record.backend.clone();
        }
        progress.update(|s| {
            s.worker_available = false;
            s.ready = false;
        })?;
        let listener = TcpListener::bind(config.address).await.map_err(|_| MigrationError::Http)?;
        let handle = if config.metrics_enabled {
            let handle = PrometheusBuilder::new()
                .install_recorder()
                .map_err(|_| MigrationError::Configuration)?;
            base_metrics::initialize_registered_metrics();
            Some(handle)
        } else {
            None
        };
        progress.update(|_| {})?;
        let http_stop = CancellationToken::new();
        let router = MigrationHttp { progress: progress.clone(), metrics: handle.clone() }.router();
        let server_stop = http_stop.clone();
        let mut server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(server_stop.cancelled_owned())
                .await
        });
        let upkeep_stop = http_stop.clone();
        let upkeep = tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(config.metrics_interval_secs));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let collector = Collector::default();
            if handle.is_some() {
                collector.describe();
            }
            loop {
                tokio::select! {
                    _ = upkeep_stop.cancelled() => break,
                    _ = ticker.tick() => if let Some(handle) = &handle { handle.run_upkeep(); collector.collect(); },
                }
            }
        });
        // HTTP failures interrupt active work; cancellation still goes through database verification.
        let operation = Self::supervise(&config, &progress, restored.map(|r| r.status.state));
        tokio::pin!(operation);
        let result = tokio::select! {
            result = &mut operation => result,
            _ = &mut server => {
                cancel.cancel();
                let _ = (&mut operation).await;
                Err(MigrationError::Http)
            }
        };
        http_stop.cancel();
        // HTTP shutdown never consumes the database cancellation budget.
        if !server.is_finished() && timeout(Duration::from_secs(2), &mut server).await.is_err() {
            server.abort();
        }
        let _ = upkeep.await;
        result
    }

    /// Initializes separate control/operation sessions and restores or runs one attempt.
    pub async fn supervise(
        config: &ManagedMigrationConfig,
        progress: &MigrationReporter,
        restored: Option<MigrationState>,
    ) -> Result<(), MigrationError> {
        let mut control = match timeout(
            Duration::from_secs(5),
            MigrationSession::connect(&config.database_url, "audit-migrate-control"),
        )
        .await
        {
            Ok(Ok(conn)) => conn,
            result => {
                let error = result
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or(MigrationError::Database { sqlstate: None });
                progress.finish(Err(error))?;
                progress.cancel.cancelled().await;
                return Ok(());
            }
        };
        if sqlx::query("SET statement_timeout='2s'").execute(&mut control).await.is_err() {
            progress.finish(Err(MigrationError::Database { sqlstate: None }))?;
            progress.cancel.cancelled().await;
            return Ok(());
        }
        let previous = progress.backend.lock().map_err(|_| MigrationError::Worker)?.clone();
        if let Some(owner) = previous
            && MigrationSession::verify_gone(&mut control, &owner, Duration::from_secs(5))
                .await
                .is_err()
        {
            // Do not kill an old session on restart or start competing work without verification.
            progress.finish(Err(MigrationError::CancellationUnconfirmed))?;
            progress.cancel.cancelled().await;
            return Err(MigrationError::CancellationUnconfirmed);
        }
        *progress.backend.lock().map_err(|_| MigrationError::Worker)? = None;
        if restored.is_some_and(|state| state != MigrationState::Running) {
            let schema_ready = AuditMigration::verify_schema(&mut control).await.is_ok();
            progress.update(|s| {
                s.schema_ready = schema_ready;
                s.worker_available = true;
                s.phase = MigrationPhase::Idle;
            })?;
            info!(state = progress.snapshot().state.label(), "restored terminal migration result");
            progress.cancel.cancelled().await;
            progress.update(|s| s.phase = MigrationPhase::Stopping)?;
            return Ok(());
        }
        if progress.cancel.is_cancelled() {
            progress.update(|s| {
                s.state = MigrationState::Stopped;
                s.cancellation_confirmed = true;
                s.phase = MigrationPhase::Stopping;
            })?;
            return Ok(());
        }
        let attempt = progress.snapshot().attempt + u64::from(restored.is_some());
        progress.update(|s| {
            *s = MigrationStatus::new(config.run_id.clone());
            s.attempt = attempt;
        })?;
        Metrics::migration_attempts_total().increment(1);
        let worker_progress = progress.clone();
        let url = config.database_url.clone();
        let mut worker = tokio::spawn(async move { Self::worker(&url, &worker_progress).await });
        tokio::select! {
            biased;
            _ = progress.cancel.cancelled() => Self::shutdown(&mut control, &mut worker, progress, config.shutdown_timeout).await,
            result = &mut worker => {
                let mut result = result.unwrap_or(Err(MigrationError::Worker));
                let owner = progress.backend.lock().map_err(|_| MigrationError::Worker)?.clone();
                if let Some(owner) = owner {
                    let cleanup = MigrationSession::stop(&mut control, &owner, Instant::now(), config.shutdown_timeout).await;
                    if cleanup.is_err() { result = Err(MigrationError::CancellationUnconfirmed); }
                }
                let terminal = progress.finish(result);
                progress.cancel.cancelled().await;
                let stopping = progress.update(|s| s.phase = MigrationPhase::Stopping);
                terminal?;
                stopping
            }
        }
    }

    /// Starts DDL only after recording ownership and durably publishing the attempt.
    pub async fn worker(url: &str, progress: &MigrationReporter) -> Result<(), MigrationError> {
        static SESSION: AtomicU64 = AtomicU64::new(0);
        let application = format!(
            "audit-migrate-{}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros(),
            SESSION.fetch_add(1, Ordering::Relaxed)
        );
        let mut conn =
            timeout(Duration::from_secs(5), MigrationSession::connect(url, &application))
                .await
                .map_err(|_| MigrationError::Database { sqlstate: None })??;
        let owner = MigrationSession::identity(&mut conn).await?;
        *progress.backend.lock().map_err(|_| MigrationError::Worker)? = Some(owner);
        let result = match progress.update(|s| s.worker_available = true) {
            Ok(()) => AuditMigration::run_cancellable(&mut conn, progress).await,
            Err(error) => Err(error),
        };
        // Do not await a new DDL operation after stop. The supervisor owns real cancellation.
        let _ = timeout(Duration::from_secs(2), conn.close()).await;
        result
    }

    /// Cancels exact-owned operations and only reports stopped after backend disappearance.
    pub async fn shutdown(
        control: &mut PgConnection,
        worker: &mut JoinHandle<Result<(), MigrationError>>,
        progress: &MigrationReporter,
        budget: Duration,
    ) -> Result<(), MigrationError> {
        let started = Instant::now();
        let deadline = started + budget;
        progress.cancel.cancel();
        // Observability/storage failure must never bypass owned database cancellation.
        let persistence = progress.update(|s| s.phase = MigrationPhase::Stopping);
        Metrics::migration_cancellation_total("requested").increment(1);
        let result = timeout_at(deadline, async {
            loop {
                let owner = progress.backend.lock().map_err(|_| MigrationError::Worker)?.clone();
                if let Some(owner) = owner {
                    MigrationSession::stop(control, &owner, started, budget).await?;
                    break;
                }
                if worker.is_finished() {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
            let _ = (&mut *worker).await;
            Ok(())
        })
        .await
        .unwrap_or(Err(MigrationError::CancellationUnconfirmed));
        if result.is_err() {
            worker.abort();
            Metrics::migration_cancellation_total("unconfirmed").increment(1);
            let _ = progress.finish(Err(MigrationError::CancellationUnconfirmed));
            return Err(MigrationError::CancellationUnconfirmed);
        }
        Metrics::migration_cancellation_total("confirmed").increment(1);
        progress.update(|s| {
            s.state = MigrationState::Stopped;
            s.complete = false;
            s.cancellation_confirmed = true;
            s.finished_at = Some(Utc::now());
        })?;
        info!("owned migration backend stop verified");
        persistence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_rejects_secret_like_identity_and_unbounded_shutdown() {
        let mut config = ManagedMigrationConfig {
            database_url: "secret".into(),
            address: "127.0.0.1:0".parse().unwrap(),
            metrics_enabled: false,
            metrics_interval_secs: 1,
            run_id: "postgres://password@host".into(),
            state_path: "state/file".into(),
            shutdown_timeout: Duration::from_secs(30),
        };
        assert_eq!(config.validate().unwrap_err().code(), "configuration");
        config.run_id = "pod-attempt-1".into();
        config.shutdown_timeout = Duration::from_secs(31);
        assert!(config.validate().is_err());
    }
}
