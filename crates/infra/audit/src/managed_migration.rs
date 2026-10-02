//! Native main-container migration supervisor with terminal hosting and verified stop.

use std::{fmt, net::SocketAddr, path::PathBuf, time::Duration};

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
    AuditMigration, Metrics, MigrationCacheWriter, MigrationDurable, MigrationDurableKey,
    MigrationError, MigrationHttp, MigrationPhase, MigrationRecord, MigrationReporter,
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
        let restored = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(MigrationError::CancellationUnconfirmed),
            restored = MigrationCacheWriter::load(store.clone(), config.run_id.clone()) => restored?,
        };
        let mut progress = MigrationReporter::new(config.run_id.clone());
        progress.cancel = cancel.clone();
        progress.store = Some(store);
        if let Some(record) = &restored {
            *progress.status.lock().map_err(|_| MigrationError::Worker)? = record.status.clone();
            // Local cache is not an authoritative result until the database record is read.
            {
                let mut status = progress.status.lock().map_err(|_| MigrationError::Worker)?;
                status.state = MigrationState::Running;
                status.phase = MigrationPhase::Starting;
            }
            *progress.backend.lock().map_err(|_| MigrationError::Worker)? = record.backend.clone();
            if let Some(target) = &record.target {
                *progress.durable.lock().map_err(|_| MigrationError::Worker)? =
                    Some(MigrationDurableKey {
                        target: target.clone(),
                        generation: config.run_id.clone(),
                    });
            }
        }
        progress
            .io(progress.update(|s| {
                s.worker_available = false;
                s.ready = false;
                s.complete = false;
                s.cleanup_confirmed = false;
                s.schema_ready = false;
            }))
            .await?;
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
        progress.io(progress.update(|_| {})).await?;
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
        let mut stop_deadline = None;
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let deadline = Instant::now() + config.shutdown_timeout;
                stop_deadline = Some(deadline);
                Self::drain_until(&mut operation, &progress, deadline, config.shutdown_timeout / 15).await
            },
            result = &mut operation => result,
            _ = &mut server => {
                cancel.cancel();
                let deadline = Instant::now() + config.shutdown_timeout;
                stop_deadline = Some(deadline);
                let _ = Self::drain_until(&mut operation, &progress, deadline, config.shutdown_timeout / 15).await;
                Err(MigrationError::Http)
            }
        };
        http_stop.cancel();
        // All exit work shares the single stop-origin deadline, including HTTP.
        let exit_deadline =
            stop_deadline.unwrap_or_else(|| Instant::now() + config.shutdown_timeout / 15);
        if !server.is_finished() && timeout_at(exit_deadline, &mut server).await.is_err() {
            server.abort();
        }
        upkeep.abort();
        result
    }

    /// Drains cancellation with reserved cache/HTTP time on one absolute deadline.
    pub async fn drain_until(
        operation: impl std::future::Future<Output = Result<(), MigrationError>>,
        progress: &MigrationReporter,
        deadline: Instant,
        reserve: Duration,
    ) -> Result<(), MigrationError> {
        match timeout_at(deadline - reserve * 2, operation).await {
            Ok(result) => result,
            Err(_) => {
                let _ = timeout_at(
                    deadline - reserve,
                    progress.finish(Err(MigrationError::CancellationUnconfirmed)),
                )
                .await;
                Err(MigrationError::CancellationUnconfirmed)
            }
        }
    }

    /// Opens independent control and hosts one durable configured retry generation.
    pub async fn supervise(
        config: &ManagedMigrationConfig,
        progress: &MigrationReporter,
        _restored: Option<MigrationState>,
    ) -> Result<(), MigrationError> {
        let mut control = match progress
            .io(MigrationSession::connect(&config.database_url, "audit-migrate-control"))
            .await
        {
            Ok(conn) => conn,
            Err(error) => {
                let published = progress.finish(Err(error)).await;
                progress.cancel.cancelled().await;
                return published;
            }
        };
        let mut gate_owned = false;
        let result = Self::durable_attempt(config, progress, &mut control, &mut gate_owned).await;
        if matches!(result, Err(MigrationError::StopRequested)) {
            let _ = timeout(config.shutdown_timeout / 15, control.close()).await;
            let _ = progress.finish(Err(MigrationError::CancellationUnconfirmed)).await;
            return Err(MigrationError::CancellationUnconfirmed);
        }
        if let Err(error) = &result {
            let key = progress.durable.lock().map_err(|_| MigrationError::Worker)?.clone();
            if let Some(key) = key.filter(|_| gate_owned && !progress.cancel.is_cancelled()) {
                let _ =
                    Self::terminal(&mut control, &key, progress, Err(error.clone()), false).await;
            } else {
                let _ = progress.finish(Err(error.clone())).await;
            }
            // Close releases an acquired gate on every error before terminal idle.
            // A restored cache key never authorizes a database write or a signal.
            let _ = timeout(config.shutdown_timeout / 15, control.close()).await;
            // A failed generation is hosted without retry, even for store/observer failure.
            progress.cancel.cancelled().await;
            return result;
        }
        let _ = timeout(config.shutdown_timeout / 15, control.close()).await;
        result
    }

    /// Serializes durable decisions separately from the worker's one schema/index lock.
    pub async fn durable_attempt(
        config: &ManagedMigrationConfig,
        progress: &MigrationReporter,
        control: &mut PgConnection,
        gate_owned: &mut bool,
    ) -> Result<(), MigrationError> {
        progress
            .io(async {
                sqlx::query("SET statement_timeout='2s'")
                    .execute(&mut *control)
                    .await
                    .map_err(|_| MigrationError::StateIo)
            })
            .await?;
        MigrationDurable::lock(control, progress).await?;
        *gate_owned = true;
        progress.check_running()?;
        // Keep the cache binding only as a comparison hint. Bootstrap failure
        // must not leave a cache-derived key available to the error writer.
        let local = progress.durable.lock().map_err(|_| MigrationError::Worker)?.take();
        let key = progress.io(MigrationDurable::bootstrap(control, &config.run_id)).await?;
        *progress.durable.lock().map_err(|_| MigrationError::Worker)? = Some(key.clone());
        if local.as_ref().is_some_and(|old| old.target != key.target) {
            // Never signal a cache owner belonging to another target.
            *progress.backend.lock().map_err(|_| MigrationError::Worker)? = None;
            return Err(MigrationError::StateCorrupt);
        }
        let restored = progress.io(MigrationDurable::load(control, &key)).await?;
        if let Some(record) = &restored {
            *progress.backend.lock().map_err(|_| MigrationError::Worker)? = record.backend.clone();
        }
        let started = Instant::now();
        // Even a new generation must not hide an unconfirmed prior owner.
        for owner in progress.io(MigrationDurable::owners(control, &key)).await? {
            let remaining = (config.shutdown_timeout * 2 / 3).saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(MigrationError::CancellationUnconfirmed);
            }
            *progress.backend.lock().map_err(|_| MigrationError::Worker)? = Some(owner.clone());
            MigrationSession::stop(control, &owner, Instant::now(), remaining).await?;
            progress.io(MigrationDurable::confirm_owner(control, &key, &owner)).await?;
        }
        if let Some(record) = &restored {
            let mut restored_status = record.status.clone();
            restored_status.state = MigrationState::Running;
            restored_status.phase = MigrationPhase::Starting;
            restored_status.ready = false;
            restored_status.complete = false;
            restored_status.worker_available = false;
            restored_status.cleanup_confirmed = false;
            *progress.status.lock().map_err(|_| MigrationError::Worker)? = restored_status;
            *progress.backend.lock().map_err(|_| MigrationError::Worker)? = record.backend.clone();
            if matches!(record.status.state, MigrationState::Succeeded | MigrationState::Failed) {
                let schema = progress
                    .io(async {
                        AuditMigration::verify_schema(control)
                            .await
                            .map_err(|error| MigrationError::database(&error))
                    })
                    .await;
                if matches!(schema, Err(MigrationError::StopRequested)) {
                    return Err(MigrationError::StopRequested);
                }
                let schema = schema.is_ok();
                progress
                    .update(|s| {
                        s.complete = false;
                        s.schema_ready = schema;
                        s.worker_available = true;
                        s.cleanup_confirmed = true;
                        s.phase = MigrationPhase::Idle;
                    })
                    .await?;
                if record.status.state == MigrationState::Succeeded {
                    let verified = progress
                        .io(async {
                            AuditMigration::verify_complete(control)
                                .await
                                .map_err(|error| MigrationError::database(&error))
                        })
                        .await;
                    if matches!(verified, Err(MigrationError::StopRequested)) {
                        return Err(MigrationError::StopRequested);
                    }
                    Self::terminal(control, &key, progress, verified, false).await?;
                } else {
                    let mut restored = record.clone();
                    restored.status.schema_ready = schema;
                    restored.status.worker_available = true;
                    restored.status.cleanup_confirmed = true;
                    restored.status.complete = false;
                    restored.status.phase = MigrationPhase::Idle;
                    MigrationDurable::save_record(control, &key, restored.clone()).await?;
                    progress.update(|s| *s = restored.status).await?;
                }
                MigrationDurable::unlock(control).await?;
                *gate_owned = false;
                progress.cancel.cancelled().await;
                return progress.update(|s| s.phase = MigrationPhase::Stopping).await;
            }
        }
        let attempt = restored.as_ref().map_or(1, |record| record.status.attempt + 1);
        progress
            .update(|s| {
                *s = MigrationStatus::new(config.run_id.clone());
                s.attempt = attempt;
            })
            .await?;
        *progress.backend.lock().map_err(|_| MigrationError::Worker)? = None;
        MigrationDurable::save(control, &key, progress).await?;
        Metrics::migration_attempts_total().increment(1);
        let worker_progress = progress.clone();
        let url = config.database_url.clone();
        let (approval, receive) = tokio::sync::oneshot::channel();
        let mut worker = tokio::spawn(async move {
            Self::worker_guarded(&url, &worker_progress, Some(receive)).await
        });
        let observed = Self::approve(control, progress, &worker).await;
        // Only the generation-gate owner persists worker ownership. The auxiliary
        // observer never writes the ledger, so a cancelled observer cannot publish
        // a delayed RUNNING record after the control's terminal commit.
        let recorded = if observed.is_ok() {
            progress.io(MigrationDurable::save(control, &key, progress)).await
        } else {
            Err(MigrationError::CancellationUnconfirmed)
        };
        let _ = approval.send(observed.is_ok() && recorded.is_ok());
        tokio::select! {
            biased;
            _ = progress.cancel.cancelled() => {
                let deadline = Instant::now() + config.shutdown_timeout * 13 / 15;
                let cleanup = Self::shutdown(control, &mut worker, progress, config.shutdown_timeout * 2 / 3).await;
                timeout_at(deadline, async {
                    Self::terminal(control, &key, progress, cleanup.clone(), cleanup.is_ok()).await?;
                    MigrationDurable::unlock(control).await
                }).await.unwrap_or(Err(MigrationError::StateIo))?;
                *gate_owned = false;
                cleanup
            }
            result = &mut worker => {
                let mut result = result.unwrap_or(Err(MigrationError::Worker));
                let owner = progress.backend.lock().map_err(|_| MigrationError::Worker)?.clone();
                let deadline = Instant::now() + config.shutdown_timeout * 13 / 15;
                let cleanup = if let Some(owner) = owner {
                    if observed.is_ok() { MigrationSession::stop(control, &owner, Instant::now(), config.shutdown_timeout * 2 / 3).await } else { Err(MigrationError::CancellationUnconfirmed) }
                } else { Err(MigrationError::CancellationUnconfirmed) };
                if cleanup.is_err() { result = Err(MigrationError::CancellationUnconfirmed); }
                if progress.update(|s| s.cleanup_confirmed = cleanup.is_ok()).await.is_err() { result = Err(MigrationError::StateIo); }
                let terminal = timeout_at(deadline, Self::terminal(control, &key, progress, result, false)).await.unwrap_or(Err(MigrationError::StateIo));
                timeout_at(deadline, MigrationDurable::unlock(control)).await.unwrap_or(Err(MigrationError::StateIo))?;
                *gate_owned = false;
                progress.cancel.cancelled().await;
                let stopping = progress.update(|s| s.phase = MigrationPhase::Stopping).await;
                terminal?;
                stopping
            }
        }
    }

    /// Persists terminal state before publishing success/stop on cached HTTP status.
    pub async fn terminal(
        control: &mut PgConnection,
        key: &MigrationDurableKey,
        progress: &MigrationReporter,
        result: Result<(), MigrationError>,
        stopped: bool,
    ) -> Result<(), MigrationError> {
        let mut status = progress.snapshot();
        let stopped = stopped
            || (progress.cancel.is_cancelled() && result.is_ok() && status.cleanup_confirmed);
        status.state = if stopped {
            MigrationState::Stopped
        } else if result.is_ok() {
            MigrationState::Succeeded
        } else {
            MigrationState::Failed
        };
        status.complete = result.is_ok() && !stopped && status.cleanup_confirmed;
        status.phase = if stopped { MigrationPhase::Stopping } else { MigrationPhase::Idle };
        status.finished_at = Some(Utc::now());
        status.partition = None;
        status.error_code = result.as_ref().err().map(|error| error.code().into());
        status.sqlstate = match &result {
            Err(MigrationError::Database { sqlstate }) => sqlstate.clone(),
            _ => None,
        };
        let record = MigrationRecord {
            fingerprint: AuditMigration::fingerprint(),
            target: Some(key.target.clone()),
            status: status.clone(),
            backend: progress.backend.lock().map_err(|_| MigrationError::Worker)?.clone(),
        };
        if MigrationDurable::save_record(control, key, record).await.is_err() {
            let _ = progress.finish(Err(MigrationError::StateIo)).await;
            return Err(MigrationError::StateIo);
        }
        let published = if stopped {
            progress.update(|s| *s = status).await
        } else {
            let finished =
                progress.finish_at(result, status.finished_at.ok_or(MigrationError::Worker)?).await;
            if finished.is_ok() { progress.update(|s| *s = status).await } else { finished }
        };
        if published.is_err() {
            // Preserve an observable cache/storage failure durably too, when possible.
            let _ = MigrationDurable::save(control, key, progress).await;
        }
        published
    }

    /// Proves the supervisor's observer sees the exact live worker BEFORE DDL.
    pub async fn approve(
        control: &mut PgConnection,
        progress: &MigrationReporter,
        worker: &JoinHandle<Result<(), MigrationError>>,
    ) -> Result<(), MigrationError> {
        timeout(Duration::from_secs(5), async {
            loop {
                progress.check_running()?;
                let owner = progress.backend.lock().map_err(|_| MigrationError::Worker)?.clone();
                if let Some(owner) = owner {
                    return if MigrationSession::exists(control, &owner).await? {
                        Ok(())
                    } else {
                        Err(MigrationError::CancellationUnconfirmed)
                    };
                }
                if worker.is_finished() {
                    return Err(MigrationError::Worker);
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or(Err(MigrationError::CancellationUnconfirmed))
    }

    /// Starts DDL only after recording ownership and durably publishing the attempt.
    pub async fn worker(url: &str, progress: &MigrationReporter) -> Result<(), MigrationError> {
        Self::worker_guarded(url, progress, None).await
    }

    /// Worker dispatch gated by the supervisor's independently verified visibility.
    pub async fn worker_guarded(
        url: &str,
        progress: &MigrationReporter,
        approval: Option<tokio::sync::oneshot::Receiver<bool>>,
    ) -> Result<(), MigrationError> {
        let application = MigrationSession::nonce();
        let mut conn =
            timeout(Duration::from_secs(5), MigrationSession::connect(url, &application))
                .await
                .map_err(|_| MigrationError::Database { sqlstate: None })??;
        let owner = progress.io(MigrationSession::identity(&mut conn)).await?;
        if owner.application != application {
            return Err(MigrationError::CancellationUnconfirmed);
        }
        *progress.backend.lock().map_err(|_| MigrationError::Worker)? = Some(owner);
        // Prove same writer/observer visibility while the worker is still alive.
        let owner = progress
            .backend
            .lock()
            .map_err(|_| MigrationError::Worker)?
            .clone()
            .ok_or(MigrationError::Worker)?;
        let mut observer = timeout(
            Duration::from_secs(5),
            MigrationSession::connect(url, "audit-migrate-observe"),
        )
        .await
        .map_err(|_| MigrationError::CancellationUnconfirmed)??;
        progress
            .io(async {
                sqlx::query("SET statement_timeout='2s'")
                    .execute(&mut observer)
                    .await
                    .map_err(|_| MigrationError::CancellationUnconfirmed)
            })
            .await?;
        if !progress.io(MigrationSession::exists(&mut observer, &owner)).await? {
            return Err(MigrationError::CancellationUnconfirmed);
        }
        let _ = timeout(Duration::from_secs(2), observer.close()).await;
        if let Some(approval) = approval {
            let approved = tokio::select! {
                biased;
                _ = progress.cancel.cancelled() => return Err(MigrationError::StopRequested),
                result = approval => result.unwrap_or(false),
            };
            if !approved {
                return Err(MigrationError::CancellationUnconfirmed);
            }
        }
        let result = match progress.update(|s| s.worker_available = true).await {
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
        Metrics::migration_cancellation_total("requested").increment(1);
        let (result, persistence) = tokio::join!(
            timeout_at(deadline, async {
                loop {
                    let owner =
                        progress.backend.lock().map_err(|_| MigrationError::Worker)?.clone();
                    if let Some(owner) = owner {
                        MigrationSession::stop(control, &owner, started, budget).await?;
                        break;
                    }
                    if worker.is_finished() {
                        return Err(MigrationError::CancellationUnconfirmed);
                    }
                    sleep(Duration::from_millis(20)).await;
                }
                let _ = (&mut *worker).await;
                Ok(())
            }),
            progress.update(|s| s.phase = MigrationPhase::Stopping)
        );
        let result = result.unwrap_or(Err(MigrationError::CancellationUnconfirmed));
        if result.is_err() {
            worker.abort();
            Metrics::migration_cancellation_total("unconfirmed").increment(1);
            let _ = progress.finish(Err(MigrationError::CancellationUnconfirmed)).await;
            return Err(MigrationError::CancellationUnconfirmed);
        }
        Metrics::migration_cancellation_total("confirmed").increment(1);
        let durable = progress.durable.lock().map_err(|_| MigrationError::Worker)?.is_some();
        progress
            .update(|s| {
                s.state = if durable { MigrationState::Running } else { MigrationState::Stopped };
                s.complete = false;
                s.cancellation_confirmed = true;
                s.cleanup_confirmed = true;
                s.finished_at = Some(Utc::now());
            })
            .await?;
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
