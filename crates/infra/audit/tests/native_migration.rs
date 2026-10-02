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
    AuditMigration, ManagedMigration, ManagedMigrationConfig, MigrationDurable, MigrationPhase,
    MigrationReporter, MigrationSession, MigrationState, MigrationStatus, MigrationStore,
    PgTransactionEventSink, TransactionEventIngestedAtIndex, index_transaction_event_partitions,
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
            .env("TIPS_AUDIT_MIGRATION_GENERATION", run_id)
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
    let saved = NativeDatabase::status(port, MigrationState::Succeeded).await?;
    assert_eq!(saved.attempt, stopped.attempt + 1);
    assert!(saved.complete && saved.ready && saved.cleanup_confirmed && saved.leaves_repaired > 0);
    NativeDatabase::signal(&mut restored, "-TERM").await?;
    let mut retry = db.child(port, "signal-two", &scratch).await?;
    let completed = NativeDatabase::status(port, MigrationState::Succeeded).await?;
    assert!(completed.complete && completed.ready && completed.leaves_built == 0);
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
    let binary = PathBuf::from(env::var("TIPS_AUDIT_TEST_BINARY")?);
    let help = Command::new(&binary)
        .arg("--help")
        .env("TIPS_AUDIT_POSTGRES_URL", &db.url)
        .output()
        .await?;
    assert!(help.status.success());
    let parse = Command::new(&binary)
        .args([
            "migrate",
            "up",
            "--managed",
            "--migration-shutdown-timeout-secs",
            "secret-password",
        ])
        .env("TIPS_AUDIT_POSTGRES_URL", &db.url)
        .output()
        .await?;
    assert!(!parse.status.success());
    for output in [help, parse] {
        for bytes in [output.stdout, output.stderr] {
            let text = String::from_utf8(bytes)?;
            for secret in ["secret-password", "private-user", "private-host", "bad-port"] {
                assert!(!text.contains(secret));
            }
        }
    }
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
async fn interrupted_restart_cleans_recorded_owner_before_resuming() -> anyhow::Result<()> {
    let db = NativeDatabase::new().await?;
    let mut old = MigrationSession::connect(&db.url, "native-prior-attempt").await?;
    let backend = MigrationSession::identity(&mut old).await?;
    let progress = MigrationReporter::new("same-pod-interrupted".into());
    *progress.backend.lock().unwrap() = Some(backend.clone());
    let mut store_conn = MigrationSession::connect(&db.url, "native-save-interrupted").await?;
    let key = MigrationDurable::bootstrap(&mut store_conn, "same-pod-interrupted").await?;
    MigrationDurable::save(&mut store_conn, &key, &progress).await?;
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
    timeout(Duration::from_secs(15), async {
        while progress.snapshot().state != MigrationState::Succeeded {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert!(progress.snapshot().cleanup_confirmed && progress.snapshot().complete);
    assert_eq!(progress.snapshot().attempt, 2);
    let mut control = MigrationSession::connect(&db.url, "native-restart-verification").await?;
    assert!(!MigrationSession::exists(&mut control, &backend).await?);
    assert!(old.ping().await.is_err());
    progress.cancel.cancel();
    timeout(Duration::from_secs(2), task).await???;
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
    for state in [MigrationState::Succeeded, MigrationState::Failed] {
        let mut saved = MigrationStatus::new("same-pod".into());
        saved.state = state;
        saved.complete = state == MigrationState::Succeeded;
        store.save(&saved, None)?;
        let mut durable = MigrationSession::connect(&db.url, "native-terminal-seed").await?;
        let key = MigrationDurable::bootstrap(&mut durable, "same-pod").await?;
        let seed = MigrationReporter::new("same-pod".into());
        *seed.status.lock().unwrap() = saved.clone();
        MigrationDurable::save(&mut durable, &key, &seed).await?;
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

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn cache_failure_during_real_signal_still_cancels_and_persists_failed_generation()
-> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut blocker = MigrationSession::connect(&db.url, "native-cache-fault-blocker").await?;
    let held = NativeDatabase::held_writer(&mut blocker).await?;
    let scratch = NativeDatabase::scratch()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut child = db.child(port, "initial", &scratch).await?;
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_progress_create_index p JOIN pg_stat_activity a ON a.pid=p.pid WHERE a.datname=current_database() AND a.application_name LIKE 'audit-migrate-%' AND p.phase LIKE 'waiting%')").await?;
    let store = MigrationStore { path: scratch.join("state.json") };
    let owner = store.load("initial")?.unwrap().backend.unwrap();
    let detached = scratch.with_extension("detached");
    fs::rename(&scratch, &detached)?;
    let pid = child.id().unwrap();
    assert!(Command::new("kill").args(["-TERM", &pid.to_string()]).status().await?.success());
    assert!(!timeout(Duration::from_secs(9), child.wait()).await??.success());
    let mut control = MigrationSession::connect(&db.url, "native-cache-fault-verify").await?;
    assert!(!MigrationSession::exists(&mut control, &owner).await?);
    let key = MigrationDurable::bootstrap(&mut control, "initial").await?;
    let failed = MigrationDurable::load(&mut control, &key).await?.unwrap();
    assert_eq!(failed.status.state, MigrationState::Failed);
    assert!(failed.status.cleanup_confirmed);
    assert_eq!(failed.status.error_code.as_deref(), Some("state_io"));
    assert_eq!(failed.backend.unwrap(), owner);
    held.rollback().await?;
    let newpod = NativeDatabase::scratch()?;
    let mut suppressed = db.child(port, "initial", &newpod).await?;
    let status = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert_eq!(status.attempt, 1);
    assert!(!status.complete && status.cleanup_confirmed);
    NativeDatabase::signal(&mut suppressed, "-TERM").await?;
    let newpod = NativeDatabase::scratch()?;
    let mut retry = db.child(port, "reviewed-2", &newpod).await?;
    let succeeded = NativeDatabase::status(port, MigrationState::Succeeded).await?;
    assert!(succeeded.complete && succeeded.cleanup_confirmed && succeeded.leaves_repaired > 0);
    NativeDatabase::signal(&mut retry, "-INT").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn wrong_definition_valid_leaf_fails_but_invalid_expected_leaf_repairs() -> anyhow::Result<()>
{
    let db = NativeDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut owner = MigrationSession::connect(&db.url, "native-definition-fixture").await?;
    let leaf: String = sqlx::query_scalar("SELECT c.relname::text FROM pg_inherits p JOIN pg_class c ON c.oid=p.inhrelid WHERE p.inhparent='transaction_events_cold'::regclass ORDER BY c.relname LIMIT 1").fetch_one(&mut owner).await?;
    let name = format!("{leaf}_ingested_at_idx");
    sqlx::query(&format!("CREATE INDEX {name} ON {leaf}(event_type)")).execute(&mut owner).await?;
    assert!(AuditMigration::run(&db.url).await.is_err());
    let still_valid: bool =
        sqlx::query_scalar("SELECT indisvalid FROM pg_index WHERE indexrelid=to_regclass($1)")
            .bind(&name)
            .fetch_one(&mut owner)
            .await?;
    assert!(still_valid, "valid wrong-definition index must remain untouched");
    sqlx::query(&format!("DROP INDEX {name}")).execute(&mut owner).await?;
    let mut held = NativeDatabase::held_writer(&mut owner).await?;
    sqlx::query("INSERT INTO transaction_events (event_id,schema_version,event_time,event_date,retention_class,producer,event_type,data) SELECT event_id || '-two',schema_version,event_time,event_date,retention_class,producer,event_type,data FROM transaction_events WHERE event_id='held'").execute(&mut *held).await?;
    held.commit().await?;
    assert!(
        sqlx::query(&format!("CREATE UNIQUE INDEX CONCURRENTLY {name} ON {leaf}(event_type)"))
            .execute(&mut owner)
            .await
            .is_err()
    );
    let invalid: bool =
        sqlx::query_scalar("SELECT NOT indisvalid FROM pg_index WHERE indexrelid=to_regclass($1)")
            .bind(&name)
            .fetch_one(&mut owner)
            .await?;
    assert!(invalid);
    AuditMigration::run(&db.url).await?;
    TransactionEventIngestedAtIndex::validate(&mut owner).await?;
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM transaction_events").fetch_one(&mut owner).await?;
    assert_eq!(rows, 2);
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn effective_role_drift_cannot_prove_owned_backend_absence() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    let mut owner = MigrationSession::connect(&db.url, "native-role-owner").await?;
    let backend = MigrationSession::identity(&mut owner).await?;
    let role = format!("native_effective_{}", chrono::Utc::now().timestamp_micros());
    sqlx::query(&format!("CREATE ROLE {role} NOLOGIN")).execute(&mut db.admin).await?;
    sqlx::query(&format!("GRANT {role} TO {}", backend.role)).execute(&mut db.admin).await?;
    let mut control = MigrationSession::connect(&db.url, "native-role-observer").await?;
    sqlx::query(&format!("SET ROLE {role}")).execute(&mut control).await?;
    assert_eq!(
        MigrationSession::exists(&mut control, &backend).await.unwrap_err().code(),
        "cancellation_unconfirmed"
    );
    assert_eq!(
        MigrationSession::signal(&mut control, &backend, true).await.unwrap_err().code(),
        "cancellation_unconfirmed"
    );
    sqlx::query("RESET ROLE").execute(&mut control).await?;
    assert!(MigrationSession::exists(&mut control, &backend).await?);
    owner.close().await?;
    MigrationSession::verify_gone(&mut control, &backend, Duration::from_secs(2)).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn copied_cache_target_change_fails_before_any_schema_dispatch() -> anyhow::Result<()> {
    let first = NativeDatabase::new().await?;
    let mut other = NativeDatabase::new().await?;
    let scratch = NativeDatabase::scratch()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut initial = first.child(port, "initial", &scratch).await?;
    NativeDatabase::status(port, MigrationState::Succeeded).await?;
    NativeDatabase::signal(&mut initial, "-TERM").await?;
    let mut changed = other.child(port, "initial", &scratch).await?;
    let failed = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert!(!failed.schema_ready && !failed.ready && !failed.complete);
    assert_eq!(failed.error_code.as_deref(), Some("state_corrupt"));
    let absent: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NULL AND to_regclass('transaction_events') IS NULL").fetch_one(&mut other.admin).await?;
    assert!(absent, "foreign-target cache cannot authorize schema or index work");
    let bound: bool = sqlx::query_scalar("SELECT target_id LIKE '%' || (SELECT oid::text FROM pg_database WHERE datname=current_database()) || ':%' AND record->'backend'='null'::jsonb FROM audit_migration_runs WHERE generation='initial'").fetch_one(&mut other.admin).await?;
    assert!(bound, "foreign cache owner must not be copied into another target record");
    let pid = changed.id().unwrap();
    assert!(Command::new("kill").args(["-TERM", &pid.to_string()]).status().await?.success());
    assert!(!timeout(Duration::from_secs(9), changed.wait()).await??.success());
    let newpod = NativeDatabase::scratch()?;
    let mut suppressed = other.child(port, "initial", &newpod).await?;
    let again = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert_eq!(again.error_code.as_deref(), Some("state_corrupt"));
    NativeDatabase::signal(&mut suppressed, "-TERM").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn unconfirmed_failed_generation_retains_and_cleans_owner_without_retry() -> anyhow::Result<()>
{
    let db = NativeDatabase::new().await?;
    let mut old = MigrationSession::connect(&db.url, "native-unconfirmed-owner").await?;
    let backend = MigrationSession::identity(&mut old).await?;
    let mut store_conn = MigrationSession::connect(&db.url, "native-unconfirmed-seed").await?;
    let key = MigrationDurable::bootstrap(&mut store_conn, "initial").await?;
    let progress = MigrationReporter::new("initial".into());
    *progress.backend.lock().unwrap() = Some(backend.clone());
    progress.finish(Err(audit_archiver_lib::MigrationError::CancellationUnconfirmed))?;
    MigrationDurable::save(&mut store_conn, &key, &progress).await?;
    let config = ManagedMigrationConfig {
        database_url: db.url.clone(),
        address: "127.0.0.1:0".parse()?,
        metrics_enabled: false,
        metrics_interval_secs: 1,
        run_id: "initial".into(),
        state_path: NativeDatabase::scratch()?.join("state.json"),
        shutdown_timeout: Duration::from_secs(9),
    };
    let cloned = progress.clone();
    let task =
        tokio::spawn(async move { ManagedMigration::supervise(&config, &cloned, None).await });
    timeout(Duration::from_secs(10), async {
        while !progress.snapshot().cleanup_confirmed {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(progress.snapshot().state, MigrationState::Failed);
    assert_eq!(progress.snapshot().attempt, 1);
    assert!(!progress.snapshot().ready && !progress.snapshot().complete);
    assert!(!MigrationSession::exists(&mut store_conn, &backend).await?);
    assert!(old.ping().await.is_err());
    timeout(Duration::from_secs(3), async {
        loop {
            if MigrationDurable::load(&mut store_conn, &key)
                .await?
                .unwrap()
                .status
                .cleanup_confirmed
            {
                return anyhow::Ok(());
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let persisted = MigrationDurable::load(&mut store_conn, &key).await?.unwrap();
    assert_eq!(persisted.backend.unwrap(), backend);
    assert!(persisted.status.cleanup_confirmed);
    let absent: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NULL AND to_regclass('transaction_events') IS NULL").fetch_one(&mut store_conn).await?;
    assert!(absent, "failed generation must clean its owner without schema/index retry");
    progress.cancel.cancel();
    timeout(Duration::from_secs(3), task).await???;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and project-local pg_dump"]
async fn native_schema_matches_committed_snapshot() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='audit_archiver')")
            .fetch_one(&mut db.admin)
            .await?;
    if !exists {
        sqlx::query("CREATE ROLE audit_archiver NOLOGIN").execute(&mut db.admin).await?;
    }
    PgTransactionEventSink::migrate(&db.url).await?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize()?;
    let binary = PathBuf::from(env::var("TIPS_AUDIT_TEST_PG_DUMP")?).canonicalize()?;
    anyhow::ensure!(binary.starts_with(root.join(".tmp")), "foreign pg_dump refused");
    let output = Command::new(binary)
        .args([
            "--schema-only",
            "--no-owner",
            "--exclude-table=_sqlx_migrations",
            "--exclude-table=transaction_events_*_2*",
            "--dbname",
            &db.url,
        ])
        .output()
        .await?;
    anyhow::ensure!(output.status.success(), "owned pg_dump failed");
    let stdout = String::from_utf8(output.stdout)?;
    let mut lines = Vec::new();
    for line in stdout.lines() {
        let preamble = ["--", "\\restrict", "\\unrestrict", "SET ", "SELECT pg_catalog.set_config"]
            .iter()
            .any(|prefix| line.starts_with(prefix));
        let repeated_blank =
            line.is_empty() && lines.last().is_none_or(|last: &&str| last.is_empty());
        if !preamble && !repeated_blank {
            lines.push(line);
        }
    }
    while lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    let header = "-- Schema produced by crates/infra/audit/migrations, excluding dated day\n-- partitions and _sqlx_migrations. Generated by the postgres_transaction_events\n-- tests; regenerate with UPDATE_SCHEMA_SNAPSHOT=1 instead of editing by hand.\n";
    let snapshot = format!("{header}\n{}\n", lines.join("\n"));
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schema.sql");
    if env::var_os("UPDATE_SCHEMA_SNAPSHOT").is_some() {
        fs::write(path, snapshot)?;
    } else {
        assert_eq!(fs::read_to_string(path)?, snapshot);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn failed_generation_survives_emptydir_loss_and_bump_retries_once() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut owner = MigrationSession::connect(&db.url, "native-invalid-unrelated").await?;
    let leaf: String = sqlx::query_scalar("SELECT c.relname::text FROM pg_inherits p JOIN pg_class c ON c.oid=p.inhrelid WHERE p.inhparent='transaction_events_cold'::regclass ORDER BY c.relname LIMIT 1").fetch_one(&mut owner).await?;
    let name = format!("{leaf}_ingested_at_idx");
    sqlx::query("CREATE TABLE unrelated_events (event_type text, ingested_at timestamptz)")
        .execute(&mut owner)
        .await?;
    sqlx::query("INSERT INTO unrelated_events VALUES('duplicate',now()),('duplicate',now())")
        .execute(&mut owner)
        .await?;
    assert!(
        sqlx::query(&format!(
            "CREATE UNIQUE INDEX CONCURRENTLY {name} ON unrelated_events(event_type)"
        ))
        .execute(&mut owner)
        .await
        .is_err()
    );
    let oid: i64 = sqlx::query_scalar("SELECT indexrelid::bigint FROM pg_index WHERE indexrelid=to_regclass($1) AND indrelid='unrelated_events'::regclass AND NOT indisvalid").bind(&name).fetch_one(&mut owner).await?;
    let scratch = NativeDatabase::scratch()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut first = db.child(port, "initial", &scratch).await?;
    let failed = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert!(failed.schema_ready && failed.cleanup_confirmed && !failed.complete);
    let untouched: i64 = sqlx::query_scalar(
        "SELECT indexrelid::bigint FROM pg_index WHERE indexrelid=to_regclass($1)",
    )
    .bind(&name)
    .fetch_one(&mut owner)
    .await?;
    assert_eq!(untouched, oid, "unrelated invalid index must never be dropped");
    NativeDatabase::signal(&mut first, "-TERM").await?;
    sqlx::query(&format!("DROP INDEX {name}")).execute(&mut owner).await?;
    let newpod = NativeDatabase::scratch()?;
    let mut restored = db.child(port, "initial", &newpod).await?;
    let suppressed = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert_eq!(suppressed.attempt, failed.attempt);
    assert_eq!(suppressed.finished_at, failed.finished_at);
    assert_eq!(suppressed.leaves_built, 0);
    assert!(suppressed.cleanup_confirmed && !suppressed.complete);
    NativeDatabase::signal(&mut restored, "-TERM").await?;
    let deliberate = NativeDatabase::scratch()?;
    let mut retry = db.child(port, "reviewed-2", &deliberate).await?;
    let completed = NativeDatabase::status(port, MigrationState::Succeeded).await?;
    assert_eq!(completed.attempt, 1);
    assert!(completed.complete && completed.cleanup_confirmed && completed.leaves_built > 0);
    NativeDatabase::signal(&mut retry, "-INT").await?;
    let repeated = NativeDatabase::scratch()?;
    let mut final_pod = db.child(port, "reviewed-2", &repeated).await?;
    let verified = NativeDatabase::status(port, MigrationState::Succeeded).await?;
    assert_eq!(verified.attempt, 1);
    assert_eq!(verified.leaves_built, completed.leaves_built);
    NativeDatabase::signal(&mut final_pod, "-TERM").await?;
    let records: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_migration_runs")
        .fetch_one(&mut db.admin)
        .await?;
    assert_eq!(records, 2);
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn restored_success_revalidates_full_catalog_without_repair() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    let scratch = NativeDatabase::scratch()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut first = db.child(port, "initial", &scratch).await?;
    let succeeded = NativeDatabase::status(port, MigrationState::Succeeded).await?;
    assert!(succeeded.complete && succeeded.cleanup_confirmed);
    NativeDatabase::signal(&mut first, "-TERM").await?;
    sqlx::query("DROP INDEX transaction_events_ingested_at_idx").execute(&mut db.admin).await?;
    let newpod = NativeDatabase::scratch()?;
    let mut restored = db.child(port, "initial", &newpod).await?;
    let failed = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert!(failed.schema_ready && failed.cleanup_confirmed && !failed.complete);
    assert_eq!(failed.attempt, succeeded.attempt);
    let absent: bool =
        sqlx::query_scalar("SELECT to_regclass('transaction_events_ingested_at_idx') IS NULL")
            .fetch_one(&mut db.admin)
            .await?;
    assert!(absent, "success restoration must not repair missing catalog work");
    NativeDatabase::signal(&mut restored, "-TERM").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn durable_target_and_fingerprint_mismatch_fail_closed() -> anyhow::Result<()> {
    let db = NativeDatabase::new().await?;
    let mut conn = MigrationSession::connect(&db.url, "native-binding-test").await?;
    let mut key = MigrationDurable::bootstrap(&mut conn, "initial").await?;
    let progress = MigrationReporter::new("initial".into());
    MigrationDurable::save(&mut conn, &key, &progress).await?;
    key.target.push_str("-wrong-target");
    assert_eq!(MigrationDurable::load(&mut conn, &key).await.unwrap_err().code(), "state_corrupt");
    key.target = key.target.trim_end_matches("-wrong-target").into();
    sqlx::query("UPDATE audit_migration_runs SET fingerprint='old-validator'")
        .execute(&mut conn)
        .await?;
    assert_eq!(MigrationDurable::load(&mut conn, &key).await.unwrap_err().code(), "state_corrupt");
    let dispatch: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(&mut conn)
        .await?;
    assert!(!dispatch, "bootstrap must not fabricate applied schema history");
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn first_schema_failure_is_durable_before_migration_history_exists() -> anyhow::Result<()> {
    let db = NativeDatabase::new().await?;
    let mut owner = MigrationSession::connect(&db.url, "native-schema-failure").await?;
    sqlx::query("CREATE TABLE transaction_events (wrong_column int)").execute(&mut owner).await?;
    let scratch = NativeDatabase::scratch()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let mut first = db.child(port, "initial", &scratch).await?;
    let failed = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert!(!failed.schema_ready && !failed.ready && failed.cleanup_confirmed);
    NativeDatabase::signal(&mut first, "-TERM").await?;
    sqlx::query("DROP TABLE transaction_events").execute(&mut owner).await?;
    let newpod = NativeDatabase::scratch()?;
    let mut second = db.child(port, "initial", &newpod).await?;
    let suppressed = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert_eq!(suppressed.attempt, 1);
    assert!(!suppressed.schema_ready && !suppressed.complete);
    let schema_absent: bool =
        sqlx::query_scalar("SELECT to_regclass('transaction_events') IS NULL")
            .fetch_one(&mut owner)
            .await?;
    assert!(schema_absent);
    NativeDatabase::signal(&mut second, "-TERM").await?;
    let newpod = NativeDatabase::scratch()?;
    let mut retry = db.child(port, "reviewed-2", &newpod).await?;
    let succeeded = NativeDatabase::status(port, MigrationState::Succeeded).await?;
    assert!(succeeded.complete && succeeded.cleanup_confirmed);
    NativeDatabase::signal(&mut retry, "-TERM").await?;
    Ok(())
}
