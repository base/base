//! Complete forward migration lifecycle and registered required online work.

use anyhow::{Result, ensure};
use sqlx::{Connection, PgConnection, migrate::Migrate};

use crate::{
    MigrationError, MigrationPhase, MigrationReporter, MigrationSession, PgTransactionEventSink,
    TransactionEventIngestedAtIndex,
};

/// Online requirements owned by upstream rather than deployment wrappers.
#[derive(Debug, Clone, Copy)]
pub enum RequiredAuditWork {
    /// BRIN indexes needed for incremental warehouse extraction.
    IngestedAt,
}

impl RequiredAuditWork {
    /// Ordered requirements checked on every full migration invocation.
    pub const ALL: [Self; 1] = [Self::IngestedAt];

    /// Stable work identifier. Change the fingerprint when its contract changes.
    pub const fn id(self) -> &'static str {
        match self {
            Self::IngestedAt => "ingested_at_brin_v1",
        }
    }

    /// Schema migration that installs this work's parent definitions.
    pub const fn prerequisite(self) -> i64 {
        match self {
            Self::IngestedAt => 2,
        }
    }

    /// Applies resumable work outside a schema transaction under the caller's lock.
    pub async fn reconcile(
        self,
        conn: &mut PgConnection,
        progress: &MigrationReporter,
    ) -> Result<usize> {
        let applied: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=$1 AND success)",
        )
        .bind(self.prerequisite())
        .fetch_one(&mut *conn)
        .await?;
        ensure!(applied, "required online work schema prerequisite missing");
        match self {
            Self::IngestedAt => TransactionEventIngestedAtIndex::reconcile(conn, progress).await,
        }
    }

    /// Validates completion from the current database catalog.
    pub async fn validate(self, conn: &mut PgConnection) -> Result<()> {
        match self {
            Self::IngestedAt => TransactionEventIngestedAtIndex::validate(conn).await,
        }
    }
}

/// Schema plus required online work, sharing a single session-level migration lock.
#[derive(Debug, Clone, Copy)]
pub struct AuditMigration;

impl AuditMigration {
    /// Runs the ordinary `migrate up` completion contract without an HTTP supervisor.
    pub async fn run(database_url: &str) -> Result<()> {
        let progress = MigrationReporter::new("ordinary".into());
        let mut conn = MigrationSession::connect(database_url, "audit-migrate-up").await?;
        let result = Self::run_on(&mut conn, &progress).await;
        let unlock = conn.unlock().await;
        let close = conn.close().await;
        result?;
        unlock?;
        close?;
        Ok(())
    }

    /// Acquires one lock, applies schema, reconciles outside its transaction, then validates.
    ///
    /// The caller must close this dedicated session on every outcome. No nested
    /// URL-taking API is called while its migration lock is held.
    pub async fn run_on(conn: &mut PgConnection, progress: &MigrationReporter) -> Result<()> {
        progress.phase(MigrationPhase::WaitingForLock)?;
        conn.lock().await?;
        progress.phase(MigrationPhase::Schema)?;
        PgTransactionEventSink::migrate_on(conn).await?;
        Self::verify_schema(conn).await?;
        progress.update(|s| s.schema_ready = true)?;
        for work in RequiredAuditWork::ALL {
            progress.phase(MigrationPhase::Reconciling)?;
            progress.update(|s| s.operation = Some(work.id().into()))?;
            work.reconcile(conn, progress).await?;
            progress.phase(MigrationPhase::Validating)?;
            work.validate(conn).await?;
        }
        progress.check_running()?;
        Ok(())
    }

    /// Read-only check of every embedded migration, including immutable checksums.
    pub async fn verify_schema(conn: &mut PgConnection) -> Result<()> {
        let exists: bool =
            sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
                .fetch_one(&mut *conn)
                .await?;
        ensure!(exists, "audit migration history missing");
        let rows: Vec<(i64, bool, Vec<u8>)> = sqlx::query_as(
            "SELECT version, success, checksum FROM public._sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut *conn)
        .await?;
        let migrator = sqlx::migrate!("./migrations");
        ensure!(rows.len() == migrator.iter().count(), "audit migration history incomplete");
        for migration in migrator.iter() {
            ensure!(
                rows.iter().any(|(version, success, checksum)| *version == migration.version
                    && *success
                    && checksum.as_slice() == migration.checksum.as_ref()),
                "audit migration history mismatch"
            );
        }
        Ok(())
    }

    /// State restoration identity includes all schema checksums and online contracts.
    pub fn fingerprint() -> String {
        let mut fingerprint = String::from("audit-v1:");
        for migration in sqlx::migrate!("./migrations").iter() {
            fingerprint.push_str(&format!("schema:{}:", migration.version));
            for byte in migration.checksum.iter() {
                fingerprint.push_str(&format!("{byte:02x}"));
            }
            fingerprint.push(';');
        }
        for work in RequiredAuditWork::ALL {
            fingerprint.push_str(&format!("work:{}:{};", work.id(), work.prerequisite()));
        }
        fingerprint
    }

    /// Stops future DDL when cancellation arrives, then lets the dedicated session close.
    ///
    /// Dropping the SQL future is only a dispatch gate, NEVER proof of server cancellation.
    /// The managed supervisor separately cancels/terminates and verifies the recorded backend.
    pub async fn run_cancellable(
        conn: &mut PgConnection,
        progress: &MigrationReporter,
    ) -> Result<(), MigrationError> {
        tokio::select! {
            biased;
            _ = progress.cancel.cancelled() => Err(MigrationError::StopRequested),
            result = Self::run_on(conn, progress) => result.map_err(|error| {
                error.downcast_ref::<MigrationError>().cloned().unwrap_or_else(|| MigrationError::database(&error))
            }),
        }
    }
}
