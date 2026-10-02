//! Dedicated operation and control sessions with exact-owned Postgres cancellation.

use std::{
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{ConnectOptions, PgConnection, Row, postgres::PgConnectOptions};
use tokio::time::{Instant, sleep, timeout, timeout_at};

use crate::MigrationError;

/// Exact server-side identity; protects unrelated sessions and PID reuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Actual writer provenance; older records without it cannot prove disappearance.
    #[serde(default)]
    pub server: Option<MigrationServer>,
}

/// Nonprivileged server provenance captured on the worker's actual SQL session.
/// A restart/failover changes this identity and invalidates an unconfirmed owner proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationServer {
    /// Actual postmaster start time, not a hostname supplied by the caller.
    pub started_at: DateTime<Utc>,
    /// Actual server-side address; None represents a Unix-domain session.
    pub address: Option<String>,
    /// Actual server-side port, not a proxy's listening port.
    pub port: Option<i32>,
    /// Database OID on this server.
    pub database_oid: i64,
}

/// Database facilities with no pooled session/lock reuse.
#[derive(Debug, Clone, Copy)]
pub struct MigrationSession;

impl MigrationSession {
    /// Same-server/login provenance evaluated in the same statement as observation/signalling.
    pub const OBSERVER: &str = "session_user=$1 AND current_user=session_user AND current_database()=$2 AND pg_postmaster_start_time()=$3 AND inet_server_addr()::text IS NOT DISTINCT FROM $4 AND inet_server_port() IS NOT DISTINCT FROM $5 AND (SELECT oid::bigint FROM pg_database WHERE datname=current_database())=$6 AND pg_backend_pid()<>$7";
    /// Migration-lock salt matching `sqlx` 0.8's `Migrate` API.
    pub const SQLX_LOCK_MULTIPLIER: i64 = 0x3d32_ad9e;

    /// Derives the exact migration key used by `sqlx` 0.8 for Postgres.
    pub fn lock_key(database: &str) -> i64 {
        const CRC_IEEE: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
        Self::SQLX_LOCK_MULTIPLIER * i64::from(CRC_IEEE.checksum(database.as_bytes()))
    }

    /// Acquires the `sqlx` session lock without keeping a waiting statement snapshot.
    /// Blocking `pg_advisory_lock` waiters can deadlock `CREATE INDEX CONCURRENTLY`'s
    /// old-snapshot wait. Each unsuccessful try completes before sleeping.
    pub async fn lock(conn: &mut PgConnection) -> Result<(), MigrationError> {
        let database: String = timeout(
            Duration::from_secs(2),
            sqlx::query_scalar("SELECT current_database()").fetch_one(&mut *conn),
        )
        .await
        .map_err(|_| MigrationError::CancellationUnconfirmed)?
        .map_err(|error| MigrationError::database(&error.into()))?;
        let key = Self::lock_key(&database);
        loop {
            let acquired: bool = timeout(
                Duration::from_secs(2),
                sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                    .bind(key)
                    .fetch_one(&mut *conn),
            )
            .await
            .map_err(|_| MigrationError::CancellationUnconfirmed)?
            .map_err(|error| MigrationError::database(&error.into()))?;
            if acquired {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Generates a bounded per-session ownership nonce, separate from retry generation.
    pub fn nonce() -> String {
        static SESSION: AtomicU64 = AtomicU64::new(0);
        format!(
            "audit-migrate-{}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros(),
            SESSION.fetch_add(1, Ordering::Relaxed)
        )
    }

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
        let row = timeout(Duration::from_secs(2), sqlx::query(
            "SELECT pid, backend_start, datname::text, usename::text, application_name, pg_postmaster_start_time() AS server_started_at, inet_server_addr()::text AS address, inet_server_port() AS port, (SELECT oid::bigint FROM pg_database WHERE datname=current_database()) AS database_oid FROM pg_stat_activity WHERE pid = pg_backend_pid()")
            .fetch_one(conn)).await.map_err(|_| MigrationError::CancellationUnconfirmed)?.map_err(|error| MigrationError::database(&error.into()))?;
        Ok(MigrationBackend {
            pid: row.try_get("pid").map_err(|_| MigrationError::CancellationUnconfirmed)?,
            started_at: row
                .try_get("backend_start")
                .map_err(|_| MigrationError::CancellationUnconfirmed)?,
            database: row
                .try_get("datname")
                .map_err(|_| MigrationError::CancellationUnconfirmed)?,
            role: row.try_get("usename").map_err(|_| MigrationError::CancellationUnconfirmed)?,
            application: row
                .try_get("application_name")
                .map_err(|_| MigrationError::CancellationUnconfirmed)?,
            server: Some(MigrationServer {
                started_at: row
                    .try_get("server_started_at")
                    .map_err(|_| MigrationError::CancellationUnconfirmed)?,
                address: row
                    .try_get("address")
                    .map_err(|_| MigrationError::CancellationUnconfirmed)?,
                port: row.try_get("port").map_err(|_| MigrationError::CancellationUnconfirmed)?,
                database_oid: row
                    .try_get("database_oid")
                    .map_err(|_| MigrationError::CancellationUnconfirmed)?,
            }),
        })
    }

    /// Tests exact-owned backend existence, not merely absence of a client socket.
    pub async fn exists(
        control: &mut PgConnection,
        owner: &MigrationBackend,
    ) -> Result<bool, MigrationError> {
        let server = owner.server.as_ref().ok_or(MigrationError::CancellationUnconfirmed)?;
        let statement = format!(
            "SELECT {}, EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$7 AND backend_start=$8 AND datname=$2 AND usename=$1 AND application_name=$9)",
            Self::OBSERVER
        );
        let (proven, present): (bool, bool) = timeout(
            Duration::from_secs(2),
            sqlx::query_as(&statement)
                .bind(&owner.role)
                .bind(&owner.database)
                .bind(server.started_at)
                .bind(&server.address)
                .bind(server.port)
                .bind(server.database_oid)
                .bind(owner.pid)
                .bind(owner.started_at)
                .bind(&owner.application)
                .fetch_one(control),
        )
        .await
        .map_err(|_| MigrationError::CancellationUnconfirmed)?
        .map_err(|_| MigrationError::CancellationUnconfirmed)?;
        if proven { Ok(present) } else { Err(MigrationError::CancellationUnconfirmed) }
    }

    /// Requires the matching login role without effective-role drift; censored absence is not proof.
    pub async fn verify_control(
        control: &mut PgConnection,
        owner: &MigrationBackend,
    ) -> Result<(), MigrationError> {
        Self::exists(control, owner).await.map(|_| ())
    }

    /// Signals only the exact owned session. A true return is NOT stop verification.
    pub async fn signal(
        control: &mut PgConnection,
        owner: &MigrationBackend,
        terminate: bool,
    ) -> Result<(), MigrationError> {
        let server = owner.server.as_ref().ok_or(MigrationError::CancellationUnconfirmed)?;
        let function = if terminate { "pg_terminate_backend" } else { "pg_cancel_backend" };
        let statement = format!(
            "SELECT allowed, CASE WHEN allowed THEN (SELECT {function}(pid) FROM pg_stat_activity WHERE pid=$7 AND backend_start=$8 AND datname=$2 AND usename=$1 AND application_name=$9) ELSE NULL END FROM (SELECT {} AS allowed) observer",
            Self::OBSERVER
        );
        let (proven, _signalled): (bool, Option<bool>) = timeout(
            Duration::from_secs(2),
            sqlx::query_as(&statement)
                .bind(&owner.role)
                .bind(&owner.database)
                .bind(server.started_at)
                .bind(&server.address)
                .bind(server.port)
                .bind(server.database_oid)
                .bind(owner.pid)
                .bind(owner.started_at)
                .bind(&owner.application)
                .fetch_one(control),
        )
        .await
        .map_err(|_| MigrationError::CancellationUnconfirmed)?
        .map_err(|_| MigrationError::CancellationUnconfirmed)?;
        if proven { Ok(()) } else { Err(MigrationError::CancellationUnconfirmed) }
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
