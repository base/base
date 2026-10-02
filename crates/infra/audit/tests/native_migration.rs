//! Native lifecycle acceptance tests against an explicitly owned disposable Postgres 17 cluster.
//!
//! Run with `TIPS_AUDIT_TEST_POSTGRES_URL` and `TIPS_AUDIT_TEST_CLUSTER_PATH` pointing
//! to the cluster created under this checkout's .tmp/, then `cargo test -p
//! audit-archiver-lib --test native_migration -- --ignored --test-threads=1`.
//! These tests refuse a foreign data directory, including localhost tunnels.

use std::{
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use audit_archiver_lib::{
    AuditMigration, ManagedMigration, ManagedMigrationConfig, MigrationPhase, MigrationReporter,
    MigrationSession, MigrationState, MigrationStatus, MigrationStore, PgTransactionEventSink,
    TransactionEventIngestedAtIndex, index_transaction_event_partitions,
};
use sqlx::{
    ConnectOptions, Connection, PgConnection, Postgres, Transaction, postgres::PgConnectOptions,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    time::{Instant, sleep, timeout},
};

/// Only an explicitly owned local PG17 data directory can authorize these tests.
#[derive(Debug)]
pub struct NativeDatabase {
    /// Unique disposable database URL for a nonsuperuser schema owner.
    pub url: String,
    /// Administration connection to this test database, not production.
    pub admin: PgConnection,
}

impl NativeDatabase {
    /// Creates a unique database and migration role within an already isolated cluster.
    pub async fn new() -> anyhow::Result<Self> {
        let url = env::var("TIPS_AUDIT_TEST_POSTGRES_URL")?;
        let options: PgConnectOptions = url.parse()?;
        anyhow::ensure!(
            matches!(options.get_host(), "127.0.0.1" | "localhost"),
            "test cluster must use loopback"
        );
        let mut admin = options.connect().await?;
        let (version, directory): (i32, String) = sqlx::query_as(
            "SELECT current_setting('server_version_num')::int, current_setting('data_directory')",
        )
        .fetch_one(&mut admin)
        .await?;
        anyhow::ensure!((170000..180000).contains(&version), "PostgreSQL 17 required");
        let expected = PathBuf::from(env::var("TIPS_AUDIT_TEST_CLUSTER_PATH")?).canonicalize()?;
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize()?;
        anyhow::ensure!(
            expected.starts_with(root.join(".tmp"))
                && PathBuf::from(directory).canonicalize()? == expected,
            "foreign test cluster refused"
        );
        let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let database = format!("native_audit_{id}");
        let role = format!("migration_{id}");
        sqlx::query(&format!("CREATE ROLE {role} LOGIN")).execute(&mut admin).await?;
        sqlx::query(&format!("CREATE DATABASE {database} OWNER {role}"))
            .execute(&mut admin)
            .await?;
        let mut admin = options.clone().database(&database).connect().await?;
        sqlx::query(&format!("ALTER SCHEMA public OWNER TO {role}")).execute(&mut admin).await?;
        let url = format!("postgres://{role}@127.0.0.1:{}/{database}", options.get_port());
        Ok(Self { url, admin })
    }

    /// Waits for an observable database trigger instead of timing a fast index build.
    pub async fn wait(&mut self, sql: &str) -> anyhow::Result<()> {
        timeout(Duration::from_secs(20), async {
            loop {
                let found: bool = sqlx::query_scalar(sql).fetch_one(&mut self.admin).await?;
                if found {
                    return anyhow::Ok(());
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        Ok(())
    }

    /// Holds a writer on the first enumerated leaf, before parent attachment can contend.
    pub async fn held_writer(conn: &mut PgConnection) -> anyhow::Result<Transaction<'_, Postgres>> {
        let day: chrono::NaiveDate = sqlx::query_scalar("SELECT to_date(right(c.relname,8),'YYYYMMDD') FROM pg_inherits p JOIN pg_class c ON c.oid=p.inhrelid WHERE p.inhparent='transaction_events_cold'::regclass ORDER BY c.relname LIMIT 1").fetch_one(&mut *conn).await?;
        let mut held = conn.begin().await?;
        sqlx::query("INSERT INTO transaction_events (event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) VALUES ('held','transaction-event/v1',$1,$2,'cold','base-builder','BUILDER_REJECTED','{}')")
            .bind(day.and_hms_opt(0,0,0).unwrap().and_utc()).bind(day).execute(&mut *held).await?;
        Ok(held)
    }

    /// Creates a dated project scratch directory for an owned child process.
    pub fn scratch() -> anyhow::Result<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.tmp")
            .join(format!("{}-native-migration-tests", chrono::Utc::now().format("%F")))
            .join(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos().to_string());
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    /// Executes the actual locally built binary with one loopback listener.
    pub async fn child(&self, port: u16, run_id: &str, path: &Path) -> anyhow::Result<Child> {
        let binary = PathBuf::from(env::var("TIPS_AUDIT_TEST_BINARY")?);
        let log = File::create(path.join(format!("{run_id}.log")))?;
        Ok(Command::new(binary)
            .args(["migrate", "up", "--managed"])
            .env("TIPS_AUDIT_POSTGRES_URL", &self.url)
            .env("TIPS_AUDIT_MIGRATION_RUN_ID", run_id)
            .env("TIPS_AUDIT_MIGRATION_STATE_PATH", path.join("state.json"))
            .env("TIPS_AUDIT_MIGRATION_SHUTDOWN_TIMEOUT_SECS", "9")
            .env("TIPS_AUDIT_METRICS_ENABLED", "true")
            .env("TIPS_AUDIT_METRICS_ADDR", "127.0.0.1")
            .env("TIPS_AUDIT_METRICS_PORT", port.to_string())
            .env("TIPS_AUDIT_LOG_FORMAT", "json")
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()?)
    }

    /// Reads an endpoint on the actual combined listener, bounded independently of DDL.
    pub async fn http(port: u16, path: &str) -> anyhow::Result<(u16, String)> {
        timeout(Duration::from_secs(2), async {
            let mut socket = TcpStream::connect(("127.0.0.1", port)).await?;
            socket
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await?;
            let mut response = String::new();
            socket.read_to_string(&mut response).await?;
            let (header, body) = response
                .split_once("\r\n\r\n")
                .ok_or_else(|| anyhow::anyhow!("invalid HTTP response"))?;
            Ok((
                header
                    .split_whitespace()
                    .nth(1)
                    .ok_or_else(|| anyhow::anyhow!("missing HTTP status"))?
                    .parse()?,
                body.into(),
            ))
        })
        .await?
    }

    /// Waits for a public operation state rather than a fixed delay.
    pub async fn status(port: u16, expected: MigrationState) -> anyhow::Result<MigrationStatus> {
        timeout(Duration::from_secs(20), async {
            loop {
                if let Ok((200, body)) = Self::http(port, "/status").await {
                    let status: MigrationStatus = serde_json::from_str(&body)?;
                    if status.state == expected {
                        return anyhow::Ok(status);
                    }
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await?
    }

    /// Sends an OS signal to the exact test-owned PID and verifies successful shutdown.
    pub async fn signal(child: &mut Child, signal: &str) -> anyhow::Result<()> {
        let pid = child.id().ok_or_else(|| anyhow::anyhow!("child exited early"))?;
        assert!(Command::new("kill").args([signal, &pid.to_string()]).status().await?.success());
        assert!(timeout(Duration::from_secs(12), child.wait()).await??.success());
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn real_term_single_listener_restart_and_deliberate_retry() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut blocker = MigrationSession::connect(&db.url, "native-signal-blocker").await?;
    let held = NativeDatabase::held_writer(&mut blocker).await?;
    let scratch = NativeDatabase::scratch()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut child = db.child(port, "signal-one", &scratch).await?;
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_progress_create_index p JOIN pg_stat_activity a ON a.pid=p.pid WHERE a.datname=current_database() AND a.application_name LIKE 'audit-migrate-%' AND p.phase LIKE 'waiting%')").await?;
    let running = NativeDatabase::status(port, MigrationState::Running).await?;
    assert!(running.ready && running.schema_ready && !running.complete);
    for path in ["/healthz", "/readyz", "/metrics"] {
        let (code, body) = NativeDatabase::http(port, path).await?;
        assert_eq!(code, 200);
        if path == "/metrics" {
            assert!(body.contains("tips_audit_migration_state"));
        }
    }
    let store = MigrationStore { path: scratch.join("state.json") };
    let owner = store.load("signal-one")?.unwrap().backend.unwrap();
    let started = Instant::now();
    NativeDatabase::signal(&mut child, "-TERM").await?;
    assert!(started.elapsed() < Duration::from_secs(9));
    let stopped = store.load("signal-one")?.unwrap().status;
    assert_eq!(stopped.state, MigrationState::Stopped);
    assert!(stopped.cancellation_confirmed && !stopped.ready);
    let mut verification = MigrationSession::connect(&db.url, "native-signal-verification").await?;
    assert!(!MigrationSession::exists(&mut verification, &owner).await?);
    let locks: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE pid=$1")
        .bind(owner.pid)
        .fetch_one(&mut db.admin)
        .await?;
    assert_eq!(locks, 0);
    held.rollback().await?;
    let mut restored = db.child(port, "signal-one", &scratch).await?;
    let saved = NativeDatabase::status(port, MigrationState::Stopped).await?;
    assert_eq!(saved.attempt, stopped.attempt);
    NativeDatabase::signal(&mut restored, "-TERM").await?;
    let mut retry = db.child(port, "signal-two", &scratch).await?;
    let completed = NativeDatabase::status(port, MigrationState::Succeeded).await?;
    assert!(completed.complete && completed.ready && completed.leaves_repaired > 0);
    NativeDatabase::signal(&mut retry, "-INT").await?;
    assert_eq!(store.load("signal-two")?.unwrap().status.state, MigrationState::Succeeded);
    assert_eq!(index_transaction_event_partitions(&db.url).await?, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn stop_during_schema_rolls_back_legacy_reset() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    let mut schema = MigrationSession::connect(&db.url, "native-legacy-setup").await?;
    sqlx::migrate!("./legacy_migrations").run(&mut schema).await?;
    let mut held = db.admin.begin().await?;
    sqlx::query("LOCK TABLE transaction_events IN ACCESS SHARE MODE").execute(&mut *held).await?;
    let progress = MigrationReporter::new("schema-stop".into());
    let worker_progress = progress.clone();
    let url = db.url.clone();
    let mut worker =
        tokio::spawn(async move { ManagedMigration::worker(&url, &worker_progress).await });
    let mut observer = MigrationSession::connect(&db.url, "native-schema-observer").await?;
    timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name LIKE 'audit-migrate-%' AND query LIKE 'DROP TABLE IF EXISTS transaction_events%' AND wait_event_type='Lock')").fetch_one(&mut observer).await?;
            if waiting { return anyhow::Ok(()); }
            sleep(Duration::from_millis(25)).await;
        }
    }).await??;
    assert_eq!(progress.snapshot().phase, MigrationPhase::Schema);
    assert!(!progress.snapshot().schema_ready);
    progress.cancel.cancel();
    ManagedMigration::shutdown(&mut observer, &mut worker, &progress, Duration::from_secs(9))
        .await?;
    held.rollback().await?;
    let versions: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&mut db.admin)
        .await?;
    assert_eq!(versions, 4, "cancelled legacy reset preserves committed history");
    let old_table: bool =
        sqlx::query_scalar("SELECT to_regclass('transaction_events') IS NOT NULL")
            .fetch_one(&mut db.admin)
            .await?;
    assert!(old_table);
    AuditMigration::run(&db.url).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn stop_during_advisory_wait_never_applies_schema() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    let mut holder = MigrationSession::connect(&db.url, "native-advisory-holder").await?;
    sqlx::migrate::Migrate::lock(&mut holder).await?;
    let progress = MigrationReporter::new("lock-stop".into());
    let cloned = progress.clone();
    let url = db.url.clone();
    let mut worker = tokio::spawn(async move { ManagedMigration::worker(&url, &cloned).await });
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name LIKE 'audit-migrate-%' AND wait_event='advisory')").await?;
    progress.cancel.cancel();
    let mut control = MigrationSession::connect(&db.url, "native-advisory-control").await?;
    ManagedMigration::shutdown(&mut control, &mut worker, &progress, Duration::from_secs(9))
        .await?;
    assert_eq!(progress.snapshot().state, MigrationState::Stopped);
    let no_schema: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NULL")
        .fetch_one(&mut db.admin)
        .await?;
    assert!(no_schema);
    sqlx::migrate::Migrate::unlock(&mut holder).await?;
    AuditMigration::run(&db.url).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn censored_control_activity_never_proves_backend_absence() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    let mut holder = MigrationSession::connect(&db.url, "native-failed-control-holder").await?;
    sqlx::migrate::Migrate::lock(&mut holder).await?;
    let progress = MigrationReporter::new("control-failure".into());
    let cloned = progress.clone();
    let url = db.url.clone();
    let mut worker = tokio::spawn(async move { ManagedMigration::worker(&url, &cloned).await });
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name LIKE 'audit-migrate-%' AND wait_event='advisory')").await?;
    progress.cancel.cancel();
    // Deliberately wrong control role: metadata absence cannot authorize a stopped result.
    let error =
        ManagedMigration::shutdown(&mut db.admin, &mut worker, &progress, Duration::from_secs(3))
            .await
            .unwrap_err();
    assert_eq!(error.code(), "cancellation_unconfirmed");
    assert_eq!(progress.snapshot().state, MigrationState::Failed);
    assert!(!progress.snapshot().cancellation_confirmed);
    assert!(!progress.snapshot().ready);
    sqlx::migrate::Migrate::unlock(&mut holder).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn malformed_secret_url_is_terminal_and_never_disclosed() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    db.url = "postgres://private-user:secret-password@private-host:bad-port/database".into();
    let scratch = NativeDatabase::scratch()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut child = db.child(port, "secret-safe", &scratch).await?;
    let failed = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert!(!failed.schema_ready && !failed.ready && !failed.complete);
    assert_eq!(NativeDatabase::http(port, "/healthz").await?.0, 200);
    assert_eq!(NativeDatabase::http(port, "/readyz").await?.0, 503);
    assert_eq!(failed.attempt, 1);
    assert!(child.try_wait()?.is_none(), "terminal failure is hosted rather than restarted");
    NativeDatabase::signal(&mut child, "-TERM").await?;
    let public = serde_json::to_string(&failed)?;
    let log = fs::read_to_string(scratch.join("secret-safe.log"))?;
    for text in [public, log] {
        for secret in ["secret-password", "private-user", "private-host", "bad-port"] {
            assert!(!text.contains(secret));
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn state_write_failure_cannot_bypass_owned_cancellation() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut blocker = MigrationSession::connect(&db.url, "native-storage-blocker").await?;
    let held = NativeDatabase::held_writer(&mut blocker).await?;
    let mut progress = MigrationReporter::new("state-io".into());
    let cloned = progress.clone();
    let url = db.url.clone();
    let mut worker = tokio::spawn(async move { ManagedMigration::worker(&url, &cloned).await });
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_progress_create_index p JOIN pg_stat_activity a ON a.pid=p.pid WHERE a.datname=current_database() AND a.application_name LIKE 'audit-migrate-%' AND p.phase LIKE 'waiting%')").await?;
    let owner = progress.backend.lock().unwrap().clone().unwrap();
    let scratch = NativeDatabase::scratch()?;
    progress.store = Some(MigrationStore { path: scratch.join("missing-parent/state.json") });
    let mut control = MigrationSession::connect(&db.url, "native-storage-control").await?;
    let error =
        ManagedMigration::shutdown(&mut control, &mut worker, &progress, Duration::from_secs(9))
            .await
            .unwrap_err();
    assert_eq!(error.code(), "state_io");
    assert!(!MigrationSession::exists(&mut control, &owner).await?);
    assert!(progress.snapshot().cancellation_confirmed);
    held.rollback().await?;
    AuditMigration::run(&db.url).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn reconcile_failure_is_live_ready_terminal_without_retry() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    AuditMigration::run(&db.url).await?;
    sqlx::query("DROP INDEX transaction_events_ingested_at_idx").execute(&mut db.admin).await?;
    let scratch = NativeDatabase::scratch()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut child = db.child(port, "reconcile-failed", &scratch).await?;
    let failed = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert!(failed.schema_ready && failed.ready && !failed.complete);
    assert_eq!(NativeDatabase::http(port, "/healthz").await?.0, 200);
    assert_eq!(NativeDatabase::http(port, "/readyz").await?.0, 200);
    assert!(child.try_wait()?.is_none());
    NativeDatabase::signal(&mut child, "-TERM").await?;
    let mut restarted = db.child(port, "reconcile-failed", &scratch).await?;
    let restored = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert_eq!(restored.attempt, failed.attempt);
    assert_eq!(restored.finished_at, failed.finished_at);
    NativeDatabase::signal(&mut restarted, "-TERM").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn termination_fallback_verifies_an_idle_owned_backend_and_its_lock_gone()
-> anyhow::Result<()> {
    let db = NativeDatabase::new().await?;
    let mut owner = MigrationSession::connect(&db.url, "native-termination-owner").await?;
    let backend = MigrationSession::identity(&mut owner).await?;
    sqlx::migrate::Migrate::lock(&mut owner).await?;
    let mut control = MigrationSession::connect(&db.url, "native-termination-control").await?;
    // Cancelling an idle backend does not end its session: fallback must terminate it.
    MigrationSession::stop(&mut control, &backend, Instant::now(), Duration::from_secs(3)).await?;
    assert!(!MigrationSession::exists(&mut control, &backend).await?);
    let error = owner.ping().await.unwrap_err();
    assert_eq!(error.as_database_error().and_then(|e| e.code()).as_deref(), Some("57P01"));
    let locks: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE pid=$1")
        .bind(backend.pid)
        .fetch_one(&mut control)
        .await?;
    assert_eq!(locks, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn interrupted_restart_fails_closed_if_recorded_backend_still_exists() -> anyhow::Result<()> {
    let db = NativeDatabase::new().await?;
    let mut old = MigrationSession::connect(&db.url, "native-prior-attempt").await?;
    let backend = MigrationSession::identity(&mut old).await?;
    let progress = MigrationReporter::new("same-pod-interrupted".into());
    *progress.backend.lock().unwrap() = Some(backend.clone());
    let scratch = NativeDatabase::scratch()?;
    let config = ManagedMigrationConfig {
        database_url: db.url.clone(),
        address: "127.0.0.1:0".parse()?,
        metrics_enabled: false,
        metrics_interval_secs: 1,
        run_id: "same-pod-interrupted".into(),
        state_path: scratch.join("state.json"),
        shutdown_timeout: Duration::from_secs(9),
    };
    let cloned = progress.clone();
    let task = tokio::spawn(async move {
        ManagedMigration::supervise(&config, &cloned, Some(MigrationState::Running)).await
    });
    timeout(Duration::from_secs(8), async {
        while progress.snapshot().state != MigrationState::Failed {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(progress.snapshot().error_code.as_deref(), Some("cancellation_unconfirmed"));
    assert!(!progress.snapshot().ready && !progress.snapshot().schema_ready);
    assert_eq!(progress.snapshot().attempt, 1);
    let mut control = MigrationSession::connect(&db.url, "native-restart-verification").await?;
    assert!(
        MigrationSession::exists(&mut control, &backend).await?,
        "restart must not kill an unverified prior session"
    );
    progress.cancel.cancel();
    assert!(timeout(Duration::from_secs(2), task).await??.is_err());
    old.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn full_up_reconciles_validates_and_repeats_without_builds() -> anyhow::Result<()> {
    let db = NativeDatabase::new().await?;
    AuditMigration::run(&db.url).await?;
    let mut conn = MigrationSession::connect(&db.url, "native-test-validation").await?;
    TransactionEventIngestedAtIndex::validate(&mut conn).await?;
    assert_eq!(index_transaction_event_partitions(&db.url).await?, 0);
    AuditMigration::run(&db.url).await?;
    let created: bool =
        sqlx::query_scalar("SELECT transaction_events_create_partition('hot', current_date + 5)")
            .fetch_one(&mut conn)
            .await?;
    assert!(created);
    TransactionEventIngestedAtIndex::validate(&mut conn).await?;
    assert_eq!(index_transaction_event_partitions(&db.url).await?, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn full_up_serializes_with_schema_only_and_index_callers() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut owner = MigrationSession::connect(&db.url, "native-test-lock").await?;
    sqlx::migrate::Migrate::lock(&mut owner).await?;
    let url = db.url.clone();
    let attempt = tokio::spawn(async move { AuditMigration::run(&url).await });
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='audit-migrate-up' AND wait_event='advisory')").await?;
    assert!(!attempt.is_finished());
    sqlx::migrate::Migrate::unlock(&mut owner).await?;
    timeout(Duration::from_secs(20), attempt).await???;
    assert_eq!(index_transaction_event_partitions(&db.url).await?, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn owned_cancel_stops_a_real_concurrent_build_and_resume_repairs() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut blocker = MigrationSession::connect(&db.url, "native-test-blocker").await?;
    let held = NativeDatabase::held_writer(&mut blocker).await?;
    let progress = MigrationReporter::new("cancel-test".into());
    let worker_progress = progress.clone();
    let url = db.url.clone();
    let mut worker =
        tokio::spawn(async move { ManagedMigration::worker(&url, &worker_progress).await });
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_progress_create_index p JOIN pg_stat_activity a ON a.pid=p.pid WHERE a.datname=current_database() AND a.application_name LIKE 'audit-migrate-%' AND p.phase LIKE 'waiting%')").await?;
    assert!(progress.snapshot().schema_ready);
    assert!(progress.snapshot().ready);
    assert!(!progress.snapshot().complete);
    let owner = progress.backend.lock().unwrap().clone().expect("ownership recorded before DDL");
    let mut unrelated = MigrationSession::connect(&db.url, "native-test-unrelated").await?;
    let other_owner = MigrationSession::identity(&mut unrelated).await?;
    let other = tokio::spawn(async move {
        let result = sqlx::query("SELECT pg_sleep(20)").execute(&mut unrelated).await;
        let _ = unrelated.close().await;
        result
    });
    let mut control = MigrationSession::connect(&db.url, "native-test-control").await?;
    let started = Instant::now();
    progress.cancel.cancel();
    ManagedMigration::shutdown(&mut control, &mut worker, &progress, Duration::from_secs(9))
        .await?;
    assert!(started.elapsed() < Duration::from_secs(9));
    assert_eq!(progress.snapshot().state, MigrationState::Stopped);
    assert!(progress.snapshot().cancellation_confirmed);
    assert!(!MigrationSession::exists(&mut control, &owner).await?);
    let progress_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_stat_progress_create_index WHERE pid=$1")
            .bind(owner.pid)
            .fetch_one(&mut db.admin)
            .await?;
    assert_eq!(progress_rows, 0);
    assert!(
        MigrationSession::exists(&mut control, &other_owner).await?,
        "unrelated same-role session survives"
    );
    assert!(!other.is_finished());
    MigrationSession::signal(&mut control, &other_owner, false).await?;
    let _ = timeout(Duration::from_secs(3), other).await??;
    let invalid: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname LIKE 'transaction_events_cold_%_ingested_at_idx' AND NOT i.indisvalid").fetch_one(&mut db.admin).await?;
    assert!(invalid > 0, "interrupted concurrent build left resumable invalid leaf");
    held.rollback().await?;
    AuditMigration::run(&db.url).await?;
    TransactionEventIngestedAtIndex::validate(&mut control).await?;
    assert_eq!(index_transaction_event_partitions(&db.url).await?, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn managed_terminal_restart_hosts_without_rerunning() -> anyhow::Result<()> {
    let db = NativeDatabase::new().await?;
    AuditMigration::run(&db.url).await?;
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.tmp")
        .join(format!("{}-native-migration-tests", chrono::Utc::now().format("%F")))
        .join(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos().to_string());
    std::fs::create_dir_all(&dir)?;
    let store = MigrationStore { path: dir.join("state.json") };
    for state in [MigrationState::Succeeded, MigrationState::Failed, MigrationState::Stopped] {
        let mut saved = MigrationStatus::new("same-pod".into());
        saved.state = state;
        saved.complete = state == MigrationState::Succeeded;
        store.save(&saved, None)?;
        let progress = MigrationReporter {
            status: Arc::new(std::sync::Mutex::new(saved)),
            store: Some(store.clone()),
            ..MigrationReporter::new("same-pod".into())
        };
        let address = TcpListener::bind("127.0.0.1:0").await?.local_addr()?;
        let config = ManagedMigrationConfig {
            database_url: db.url.clone(),
            address,
            metrics_enabled: false,
            metrics_interval_secs: 1,
            run_id: "same-pod".into(),
            state_path: store.path.clone(),
            shutdown_timeout: Duration::from_secs(9),
        };
        let cloned = progress.clone();
        let task = tokio::spawn(async move {
            ManagedMigration::supervise(&config, &cloned, Some(state)).await
        });
        timeout(Duration::from_secs(5), async {
            while !progress.snapshot().worker_available {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        assert_eq!(progress.snapshot().state, state);
        assert_eq!(progress.snapshot().attempt, 1);
        assert!(progress.snapshot().ready);
        assert_eq!(progress.snapshot().phase, MigrationPhase::Idle);
        progress.cancel.cancel();
        timeout(Duration::from_secs(3), task).await???;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn invalid_parent_contract_fails_without_rewriting_applied_history() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    AuditMigration::run(&db.url).await?;
    let history: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut db.admin)
            .await?;
    sqlx::query("DROP INDEX transaction_events_ingested_at_idx").execute(&mut db.admin).await?;
    assert!(AuditMigration::run(&db.url).await.is_err());
    let after: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut db.admin)
            .await?;
    assert_eq!(history, after);
    Ok(())
}
