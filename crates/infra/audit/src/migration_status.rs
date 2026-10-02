//! Secret-safe migration status, progress, and error classification.

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{Metrics, MigrationBackend, MigrationDurableKey, MigrationStore};

/// Observable operation result; a terminal result never automatically retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationState {
    /// An attempt is starting or executing.
    Running,
    /// All registered requirements passed catalog validation.
    Succeeded,
    /// The attempt failed and remains idle.
    Failed,
    /// Shutdown verified that the owned database backend disappeared.
    Stopped,
}

impl MigrationState {
    /// Every bounded metric value.
    pub const ALL: [Self; 4] = [Self::Running, Self::Succeeded, Self::Failed, Self::Stopped];

    /// Stable metric label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// Current step, independent of operation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    /// HTTP is available; database initialization is pending.
    Starting,
    /// Waiting for the database's migration advisory lock.
    WaitingForLock,
    /// Applying immutable schema migrations.
    Schema,
    /// Reconciling required online work outside schema transactions.
    Reconciling,
    /// Checking required catalog invariants.
    Validating,
    /// Hosting a terminal result without retrying.
    Idle,
    /// Cancelling and verifying owned database work.
    Stopping,
}

impl MigrationPhase {
    /// Every bounded metric value.
    pub const ALL: [Self; 7] = [
        Self::Starting,
        Self::WaitingForLock,
        Self::Schema,
        Self::Reconciling,
        Self::Validating,
        Self::Idle,
        Self::Stopping,
    ];

    /// Stable metric label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::WaitingForLock => "waiting_for_lock",
            Self::Schema => "schema",
            Self::Reconciling => "reconciling",
            Self::Validating => "validating",
            Self::Idle => "idle",
            Self::Stopping => "stopping",
        }
    }
}

/// Errors deliberately omit connection strings, driver messages, and OS paths.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MigrationError {
    /// Invalid CLI or environment configuration.
    #[error("migration configuration invalid")]
    Configuration,
    /// A state record could not be durably read or written.
    #[error("migration state IO failed")]
    StateIo,
    /// A corrupt or incompatible state record must not authorize DDL.
    #[error("migration state incompatible; use a deliberate new run identity")]
    StateCorrupt,
    /// A database operation failed; only validated SQLSTATE is exposed.
    #[error("migration database operation failed")]
    Database {
        /// Five-character Postgres error class, without driver text.
        sqlstate: Option<String>,
    },
    /// Cooperative stop was requested.
    #[error("migration stop requested")]
    StopRequested,
    /// Owned server work could not be proven stopped within the budget.
    #[error("migration cancellation unconfirmed")]
    CancellationUnconfirmed,
    /// The supervisor lost its HTTP listener.
    #[error("migration HTTP server failed")]
    Http,
    /// The worker task panicked or otherwise failed to join.
    #[error("migration worker failed")]
    Worker,
}

impl MigrationError {
    /// Stable, safe error code for status and logs.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::StateIo => "state_io",
            Self::StateCorrupt => "state_corrupt",
            Self::Database { .. } => "database",
            Self::StopRequested => "stop_requested",
            Self::CancellationUnconfirmed => "cancellation_unconfirmed",
            Self::Http => "http",
            Self::Worker => "worker",
        }
    }

    /// Classifies database errors without preserving potentially secret text.
    pub fn database(error: &anyhow::Error) -> Self {
        let sqlstate = error.chain().find_map(|cause| {
            cause
                .downcast_ref::<sqlx::Error>()?
                .as_database_error()?
                .code()
                .filter(|code| {
                    code.len() == 5
                        && code.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                })
                .map(|code| code.into_owned())
        });
        Self::Database { sqlstate }
    }
}

/// Versioned public snapshot. Readiness measures availability, not completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationStatus {
    /// JSON contract version.
    pub version: u8,
    /// Command contract.
    pub mode: String,
    /// Nonsensitive attempt identity supplied by deployment.
    pub run_id: String,
    /// Number of attempts in this pod's state record.
    pub attempt: u64,
    /// Terminal or running operation state.
    pub state: MigrationState,
    /// Current lifecycle phase.
    pub phase: MigrationPhase,
    /// All embedded schema migrations were committed and verified.
    pub schema_ready: bool,
    /// The supervisor can host the worker and its terminal result.
    pub worker_available: bool,
    /// Availability independently of required index completion.
    pub ready: bool,
    /// True only after every required catalog invariant passes.
    pub complete: bool,
    /// Owned database backend absence has been verified on stop.
    pub cancellation_confirmed: bool,
    /// Exact owned backend disappearance verified for every terminal result.
    #[serde(default)]
    pub cleanup_confirmed: bool,
    /// Attempt start time.
    pub started_at: DateTime<Utc>,
    /// Terminal result time.
    pub finished_at: Option<DateTime<Utc>>,
    /// Last actual progress time.
    pub updated_at: DateTime<Utc>,
    /// Stable registered work identifier.
    pub operation: Option<String>,
    /// Validated day partition currently being processed.
    pub partition: Option<String>,
    /// Attached day tables enumerated in the current pass.
    pub leaves_total: u64,
    /// Day tables completed in the current pass.
    pub leaves_completed: u64,
    /// Leaf indexes built in this attempt.
    pub leaves_built: u64,
    /// Invalid unattached leaf indexes removed in this attempt.
    pub leaves_repaired: u64,
    /// Existing attached indexes verified in this attempt.
    pub leaves_skipped: u64,
    /// Safe error classification, never raw driver text.
    pub error_code: Option<String>,
    /// Validated Postgres error code.
    pub sqlstate: Option<String>,
}

impl MigrationStatus {
    /// Creates a starting snapshot before any database work.
    pub fn new(run_id: String) -> Self {
        let now = Utc::now();
        Self {
            version: 1,
            mode: "migrate_up".into(),
            run_id,
            attempt: 1,
            state: MigrationState::Running,
            phase: MigrationPhase::Starting,
            schema_ready: false,
            worker_available: false,
            ready: false,
            complete: false,
            cancellation_confirmed: false,
            cleanup_confirmed: false,
            started_at: now,
            finished_at: None,
            updated_at: now,
            operation: None,
            partition: None,
            leaves_total: 0,
            leaves_completed: 0,
            leaves_built: 0,
            leaves_repaired: 0,
            leaves_skipped: 0,
            error_code: None,
            sqlstate: None,
        }
    }
}

/// Shared cached snapshots and cooperative stop gate; no probe uses the DDL session.
#[derive(Clone)]
pub struct MigrationReporter {
    /// Cached public snapshot.
    pub status: Arc<Mutex<MigrationStatus>>,
    /// Cooperative stop request.
    pub cancel: CancellationToken,
    /// Optional durable same-pod record.
    pub store: Option<MigrationStore>,
    /// Owned backend persisted separately from public HTTP status.
    pub backend: Arc<Mutex<Option<MigrationBackend>>>,
    /// Authoritative database record identity, not exposed by HTTP.
    pub durable: Arc<Mutex<Option<MigrationDurableKey>>>,
}

impl fmt::Debug for MigrationReporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MigrationReporter { ownership and paths redacted }")
    }
}

impl MigrationReporter {
    /// Creates an in-memory reporter for ordinary migration callers.
    pub fn new(run_id: String) -> Self {
        Self {
            status: Arc::new(Mutex::new(MigrationStatus::new(run_id))),
            cancel: CancellationToken::new(),
            store: None,
            backend: Arc::new(Mutex::new(None)),
            durable: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns a consistent cached public status.
    pub fn snapshot(&self) -> MigrationStatus {
        let mut status = self.status.lock().expect("migration snapshot lock poisoned").clone();
        if self.cancel.is_cancelled() {
            status.ready = false;
            status.phase = MigrationPhase::Stopping;
        }
        status
    }

    /// Updates and persists observable progress before publishing metrics.
    pub fn update(&self, change: impl FnOnce(&mut MigrationStatus)) -> Result<(), MigrationError> {
        let mut status = self.status.lock().map_err(|_| MigrationError::Worker)?;
        change(&mut status);
        status.updated_at = Utc::now();
        status.ready = status.schema_ready
            && !self.cancel.is_cancelled()
            && status.worker_available
            && status.phase != MigrationPhase::Stopping
            && !matches!(
                status.error_code.as_deref(),
                Some("cancellation_unconfirmed" | "state_io" | "state_corrupt")
            );
        let persisted = if let Some(store) = &self.store {
            let target = self.durable.lock().map_err(|_| MigrationError::Worker)?;
            store.save_bound(
                &status,
                self.backend.lock().map_err(|_| MigrationError::Worker)?.as_ref(),
                target.as_ref().map(|key| key.target.as_str()),
            )
        } else {
            Ok(())
        };
        if persisted.is_err() {
            status.state = MigrationState::Failed;
            status.ready = false;
            status.complete = false;
            if status.error_code.as_deref() != Some("cancellation_unconfirmed") {
                status.error_code = Some("state_io".into());
            }
        }
        for state in MigrationState::ALL {
            Metrics::migration_state(state.label()).set(f64::from(status.state == state));
        }
        for phase in MigrationPhase::ALL {
            Metrics::migration_phase(phase.label()).set(f64::from(status.phase == phase));
        }
        Metrics::migration_schema_ready().set(f64::from(status.schema_ready));
        Metrics::migration_worker_available().set(f64::from(status.worker_available));
        Metrics::migration_complete().set(f64::from(status.complete));
        Metrics::migration_cleanup_confirmed().set(f64::from(status.cleanup_confirmed));
        Metrics::migration_leaves_total().set(status.leaves_total as f64);
        Metrics::migration_leaves_completed().set(status.leaves_completed as f64);
        Metrics::migration_last_progress_timestamp_seconds()
            .set(status.updated_at.timestamp() as f64);
        persisted
    }

    /// Moves to a step only while no shutdown was requested.
    pub fn phase(&self, phase: MigrationPhase) -> Result<(), MigrationError> {
        self.check_running()?;
        self.update(|s| s.phase = phase)?;
        info!(phase = phase.label(), "migration phase changed");
        Ok(())
    }

    /// Prevents further work after stop is requested.
    pub fn check_running(&self) -> Result<(), MigrationError> {
        if self.cancel.is_cancelled() { Err(MigrationError::StopRequested) } else { Ok(()) }
    }

    /// Publishes a terminal result without raw error chains.
    pub fn finish(&self, result: Result<(), MigrationError>) -> Result<(), MigrationError> {
        self.finish_at(result, Utc::now())
    }

    /// Publishes the exact durable terminal timestamp, without a transient different result.
    pub fn finish_at(
        &self,
        result: Result<(), MigrationError>,
        finished_at: DateTime<Utc>,
    ) -> Result<(), MigrationError> {
        let phase = self.snapshot().phase;
        let persisted = self.update(|s| {
            s.state =
                if result.is_ok() { MigrationState::Succeeded } else { MigrationState::Failed };
            s.complete = result.is_ok();
            s.finished_at = Some(finished_at);
            s.partition = None;
            s.phase = MigrationPhase::Idle;
            if let Err(error) = &result {
                s.error_code = Some(error.code().into());
                if let MigrationError::Database { sqlstate } = error {
                    s.sqlstate.clone_from(sqlstate);
                }
            }
        });
        let status = self.snapshot();
        if status.state == MigrationState::Failed {
            Metrics::migration_failures_total(phase.label()).increment(1);
        }
        Metrics::migration_duration_seconds()
            .record((status.updated_at - status.started_at).num_milliseconds() as f64 / 1000.0);
        info!(
            state = status.state.label(),
            error_code = status.error_code.as_deref().unwrap_or("none"),
            "migration attempt finished"
        );
        persisted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_failure_never_publishes_success_or_readiness() {
        let mut progress = MigrationReporter::new("storage-failure".into());
        progress.store = Some(MigrationStore {
            path: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../.tmp")
                .join(format!(
                    "{}-audit-state-unit-{}",
                    Utc::now().format("%F"),
                    Utc::now().timestamp_nanos_opt().unwrap()
                ))
                .join("missing-directory/state.json"),
        });
        assert_eq!(progress.finish(Ok(())).unwrap_err().code(), "state_io");
        assert_eq!(progress.snapshot().state, MigrationState::Failed);
        assert!(!progress.snapshot().ready && !progress.snapshot().complete);
    }

    #[test]
    fn driver_error_text_never_enters_status() {
        let secret = "postgres://user:super-secret@private-host/database";
        let error = anyhow::Error::from(sqlx::Error::Configuration(secret.into()));
        let reporter = MigrationReporter::new("test".into());
        reporter.finish(Err(MigrationError::database(&error))).unwrap();
        let json = serde_json::to_string(&reporter.snapshot()).unwrap();
        assert!(!json.contains("super-secret"));
        assert!(!json.contains("private-host"));
        assert_eq!(reporter.snapshot().error_code.as_deref(), Some("database"));
    }

    #[test]
    fn readiness_precedes_completion_and_survives_reconcile_failure() {
        let reporter = MigrationReporter::new("test".into());
        reporter
            .update(|s| {
                s.schema_ready = true;
                s.worker_available = true;
                s.phase = MigrationPhase::Reconciling;
            })
            .unwrap();
        assert!(reporter.snapshot().ready);
        assert!(!reporter.snapshot().complete);
        reporter.finish(Err(MigrationError::Database { sqlstate: None })).unwrap();
        assert!(reporter.snapshot().ready);
        assert_eq!(reporter.snapshot().state, MigrationState::Failed);
    }
}
