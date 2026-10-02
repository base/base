//! Dedicated operation and control sessions with exact-owned Postgres cancellation.

use std::{str::FromStr, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{ConnectOptions, PgConnection, postgres::PgConnectOptions};
use tokio::time::{Instant, sleep, timeout_at};

use crate::MigrationError;

/// Exact server-side identity; protects unrelated sessions and PID reuse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationBackend {
    /// Owned server PID.
    pub pid: i32,
    /// Server-recorded backend creation time.
    pub started_at: DateTime<Utc>,
    /// Database owning the operation.
    pub database: String,
    /// Role owning the operation.
    pub role: String,
    /// Generated per-session application identifier, not user-provided text.
    pub application: String,
}

/// Database facilities with no pooled session/lock reuse.
#[derive(Debug, Clone, Copy)]
pub struct MigrationSession;

impl MigrationSession {
    /// Opens a dedicated session, disabling statement logging and secret-bearing errors.
    pub async fn connect(url: &str, application: &str) -> Result<PgConnection, MigrationError> {
        let options = PgConnectOptions::from_str(url)
            .map_err(|_| MigrationError::Configuration)?
            .application_name(application)
            .log_statements(log::LevelFilter::Off)
            .log_slow_statements(log::LevelFilter::Off, Duration::from_secs(1));
        options.connect().await.map_err(|error| MigrationError::database(&error.into()))
    }

    /// Captures identity before lock acquisition or any DDL.
    pub async fn identity(conn: &mut PgConnection) -> Result<MigrationBackend, MigrationError> {
        let row: (i32, DateTime<Utc>, String, String, String) = sqlx::query_as(
            "SELECT pid, backend_start, datname::text, usename::text, application_name FROM pg_stat_activity WHERE pid = pg_backend_pid()")
            .fetch_one(conn).await.map_err(|error| MigrationError::database(&error.into()))?;
        Ok(MigrationBackend {
            pid: row.0,
            started_at: row.1,
            database: row.2,
            role: row.3,
            application: row.4,
        })
    }

    /// Tests exact-owned backend existence, not merely absence of a client socket.
    pub async fn exists(
        control: &mut PgConnection,
        owner: &MigrationBackend,
    ) -> Result<bool, MigrationError> {
        Self::verify_control(control, owner).await?;
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND backend_start=$2 AND datname=$3 AND usename=$4 AND application_name=$5)")
            .bind(owner.pid).bind(owner.started_at).bind(&owner.database).bind(&owner.role).bind(&owner.application)
            .fetch_one(control).await.map_err(|error| MigrationError::database(&error.into()))
    }

    /// A different role can see censored activity columns; absence then is not evidence.
    pub async fn verify_control(
        control: &mut PgConnection,
        owner: &MigrationBackend,
    ) -> Result<(), MigrationError> {
        let visible: bool = sqlx::query_scalar("SELECT current_user=$1 AND current_database()=$2")
            .bind(&owner.role)
            .bind(&owner.database)
            .fetch_one(control)
            .await
            .map_err(|_| MigrationError::CancellationUnconfirmed)?;
        if visible { Ok(()) } else { Err(MigrationError::CancellationUnconfirmed) }
    }

    /// Signals only the exact owned session. A true return is NOT stop verification.
    pub async fn signal(
        control: &mut PgConnection,
        owner: &MigrationBackend,
        terminate: bool,
    ) -> Result<(), MigrationError> {
        Self::verify_control(control, owner).await?;
        let statement = if terminate {
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE pid=$1 AND backend_start=$2 AND datname=$3 AND usename=$4 AND application_name=$5"
        } else {
            "SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE pid=$1 AND backend_start=$2 AND datname=$3 AND usename=$4 AND application_name=$5"
        };
        let _: Option<bool> = sqlx::query_scalar(statement)
            .bind(owner.pid)
            .bind(owner.started_at)
            .bind(&owner.database)
            .bind(&owner.role)
            .bind(&owner.application)
            .fetch_optional(control)
            .await
            .map_err(|error| MigrationError::database(&error.into()))?;
        Ok(())
    }

    /// Repeats cancellation, then termination, within an absolute shutdown deadline.
    ///
    /// Backend disappearance also proves its progress row and session locks are gone.
    /// If the control connection is lost, no false stopped state is returned.
    pub async fn stop(
        control: &mut PgConnection,
        owner: &MigrationBackend,
        started: Instant,
        budget: Duration,
    ) -> Result<(), MigrationError> {
        let cancel_until = started + budget / 3;
        let signal_until = started + budget * 2 / 3;
        let deadline = started + budget;
        timeout_at(deadline, async {
            loop {
                if !Self::exists(control, owner).await? {
                    return Ok(());
                }
                let now = Instant::now();
                if now < signal_until {
                    Self::signal(control, owner, now >= cancel_until).await?;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or(Err(MigrationError::CancellationUnconfirmed))
        .map_err(|_| MigrationError::CancellationUnconfirmed)
    }

    /// Bounded read-only verification used after natural completion or restart.
    pub async fn verify_gone(
        control: &mut PgConnection,
        owner: &MigrationBackend,
        budget: Duration,
    ) -> Result<(), MigrationError> {
        timeout_at(Instant::now() + budget, async {
            while Self::exists(control, owner).await? {
                sleep(Duration::from_millis(50)).await;
            }
            Ok(())
        })
        .await
        .unwrap_or(Err(MigrationError::CancellationUnconfirmed))
    }
}
