//! Native lifecycle acceptance tests against an explicitly owned disposable Postgres 17 cluster.
//!
//! Run with `TIPS_AUDIT_TEST_POSTGRES_URL` and `TIPS_AUDIT_TEST_CLUSTER_PATH` pointing
//! to the cluster created under this checkout's .tmp/, then `cargo test -p
//! audit-archiver-lib --test native_migration -- --ignored --test-threads=1`.
//! These tests refuse a foreign data directory, including localhost tunnels.
//! The before/after ledger regression additionally requires
//! `TIPS_AUDIT_TEST_BASELINE_BINARY` pointing to the preserved f44a4c237 binary.

use std::{
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use audit_archiver_lib::{
    AuditMigration, ManagedMigration, ManagedMigrationConfig, MigrationDurable, MigrationPhase,
    MigrationReporter, MigrationSession, MigrationState, MigrationStatus, MigrationStore,
    PgTransactionEventSink, TransactionEventIngestedAtIndex, index_transaction_event_partitions,
};
use audit_archiver_lib::{MigrationCacheWriter, MigrationHttp};
use sqlx::{
    ConnectOptions, Connection, PgConnection, Postgres, Transaction, postgres::PgConnectOptions,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

/// Owned loopback proxy that forwards requests but discards a selected query's responses.
/// It models a response blackhole without relying on server-side statement timeouts.
#[derive(Debug)]
pub struct NativeResponseProxy {
    /// URL with only this owned listener's port changed.
    pub url: String,
    /// Confirms that the selected query was forwarded before response loss.
    pub triggered: CancellationToken,
    /// Listener lifetime; accepted connections end when their owned child closes.
    pub task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Drop for NativeResponseProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl NativeResponseProxy {
    /// Starts a session-transparent response-dropping proxy to the already verified cluster.
    pub async fn start(url: &str, query: &'static [u8]) -> anyhow::Result<Self> {
        Self::start_inner(url, query, false).await
    }

    /// Holds only auxiliary observer upstream sockets after their client closes.
    /// This models a session proxy retaining a dispatched SQL write, not cancellation.
    pub async fn start_inner(
        url: &str,
        query: &'static [u8],
        linger_auxiliary: bool,
    ) -> anyhow::Result<Self> {
        let options: PgConnectOptions = url.parse()?;
        let backend = options.get_port();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let proxy_url = format!(
            "postgres://{}@127.0.0.1:{port}/{}",
            options.get_username(),
            options.get_database().unwrap()
        );
        let triggered = CancellationToken::new();
        let observed = triggered.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut client, _) = listener.accept().await?;
                let observed = observed.clone();
                tokio::spawn(async move {
                    let mut server = TcpStream::connect(("127.0.0.1", backend)).await?;
                    let mut requests = [0u8; 4096];
                    let mut responses = [0u8; 4096];
                    let mut tail = Vec::new();
                    let mut dropping = false;
                    let mut auxiliary = false;
                    loop {
                        tokio::select! {
                            read = client.read(&mut requests) => {
                                let count = match read {
                                    Ok(count) => count,
                                    Err(error) => {
                                        if linger_auxiliary && auxiliary { sleep(Duration::from_secs(3)).await; }
                                        return Err(anyhow::Error::from(error));
                                    }
                                };
                                if count == 0 {
                                    if linger_auxiliary && auxiliary { sleep(Duration::from_secs(3)).await; }
                                    return anyhow::Ok(());
                                }
                                tail.extend_from_slice(&requests[..count]);
                                auxiliary |= tail.windows(b"audit-migrate-observe".len()).any(|bytes| bytes == b"audit-migrate-observe");
                                if tail.windows(query.len()).any(|bytes| bytes == query) {
                                    dropping = true;
                                    observed.cancel();
                                }
                                server.write_all(&requests[..count]).await?;
                                if tail.len() > 1024 { tail.drain(..tail.len() - 1024); }
                            },
                            read = server.read(&mut responses) => {
                                let count = read?;
                                if count == 0 { return anyhow::Ok(()); }
                                if !dropping && let Err(error) = client.write_all(&responses[..count]).await {
                                    // A closed downstream can race EOF delivery. Keep the
                                    // upstream alive on BOTH paths, not only client.read(0).
                                    if linger_auxiliary && auxiliary { sleep(Duration::from_secs(3)).await; }
                                    return Err(anyhow::Error::from(error));
                                }
                            },
                        }
                    }
                });
            }
        });
        Ok(Self { url: proxy_url, triggered, task })
    }
}

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

    /// Seeds a fixture ledger using the same session-owned generation gate as native code.
    pub async fn save(
        conn: &mut PgConnection,
        key: &audit_archiver_lib::MigrationDurableKey,
        progress: &MigrationReporter,
    ) -> anyhow::Result<()> {
        MigrationDurable::lock(conn, progress).await?;
        MigrationDurable::save(conn, key, progress).await?;
        MigrationDurable::unlock(conn).await?;
        Ok(())
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
        self.child_binary(&binary, port, run_id, path).await
    }

    /// Runs a preserved baseline binary for before/after regression evidence.
    pub async fn child_binary(
        &self,
        binary: &Path,
        port: u16,
        run_id: &str,
        path: &Path,
    ) -> anyhow::Result<Child> {
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
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name LIKE 'audit-migrate-%' AND application_name NOT IN ('audit-migrate-control','audit-migrate-observe') AND query LIKE '%pg_try_advisory_lock%')").await?;
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
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name LIKE 'audit-migrate-%' AND application_name NOT IN ('audit-migrate-control','audit-migrate-observe') AND query LIKE '%pg_try_advisory_lock%')").await?;
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
async fn response_blackhole_during_setup_or_restore_has_bounded_real_signal_exit()
-> anyhow::Result<()> {
    for restore in [false, true] {
        let mut db = NativeDatabase::new().await?;
        let scratch = NativeDatabase::scratch()?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let generation = if restore { "blackhole-restore" } else { "blackhole-set" };
        if restore {
            let mut initial = db.child(port, generation, &scratch).await?;
            NativeDatabase::status(port, MigrationState::Succeeded).await?;
            NativeDatabase::signal(&mut initial, "-INT").await?;
        }
        let query: &'static [u8] = if restore {
            b"SELECT version, success, checksum FROM public._sqlx_migrations"
        } else {
            b"SET statement_timeout='2s'"
        };
        let proxy = NativeResponseProxy::start(&db.url, query).await?;
        db.url.clone_from(&proxy.url);
        let path = NativeDatabase::scratch()?;
        let mut child = db.child(port, generation, &path).await?;
        timeout(Duration::from_secs(15), proxy.triggered.cancelled()).await?;
        let (code, body) = NativeDatabase::http(port, "/status").await?;
        assert_eq!(code, 200);
        let pending: MigrationStatus = serde_json::from_str(&body)?;
        assert!(!pending.ready && !pending.complete && !pending.cleanup_confirmed);
        assert_eq!(NativeDatabase::http(port, "/healthz").await?.0, 200);
        assert_eq!(NativeDatabase::http(port, "/readyz").await?.0, 503);
        let pid = child.id().unwrap().to_string();
        let started = Instant::now();
        assert!(
            Command::new("kill")
                .args([if restore { "-INT" } else { "-TERM" }, &pid])
                .status()
                .await?
                .success()
        );
        let result = timeout(Duration::from_secs(9), child.wait()).await??;
        assert!(!result.success(), "unobserved cleanup must fail closed");
        assert!(started.elapsed() < Duration::from_secs(9));
        let record = MigrationStore { path: path.join("state.json") }.load(generation)?.unwrap();
        assert_eq!(record.status.state, MigrationState::Failed);
        assert!(
            !record.status.complete
                && !record.status.cleanup_confirmed
                && !record.status.cancellation_confirmed
        );
        assert_eq!(record.status.error_code.as_deref(), Some("cancellation_unconfirmed"));
        let exists: bool =
            sqlx::query_scalar("SELECT to_regclass('public.transaction_events') IS NOT NULL")
                .fetch_one(&mut db.admin)
                .await?;
        assert_eq!(exists, restore, "setup response loss cannot authorize schema DDL");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires two explicitly owned disposable PostgreSQL 17 clusters"]
async fn same_named_wrong_server_cannot_approve_signal_or_prove_owned_absence() -> anyhow::Result<()>
{
    let db = NativeDatabase::new().await?;
    let url = env::var("TIPS_AUDIT_TEST_OBSERVER_POSTGRES_URL")?;
    let options: PgConnectOptions = url.parse()?;
    anyhow::ensure!(options.get_host() == "127.0.0.1", "observer cluster must be loopback");
    let mut admin = options.connect().await?;
    let (version, directory): (i32, String) = sqlx::query_as(
        "SELECT current_setting('server_version_num')::int,current_setting('data_directory')",
    )
    .fetch_one(&mut admin)
    .await?;
    let expected =
        PathBuf::from(env::var("TIPS_AUDIT_TEST_OBSERVER_CLUSTER_PATH")?).canonicalize()?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize()?;
    anyhow::ensure!(
        (170000..180000).contains(&version)
            && expected.starts_with(root.join(".tmp"))
            && PathBuf::from(directory).canonicalize()? == expected,
        "foreign observer cluster refused"
    );
    let source: PgConnectOptions = db.url.parse()?;
    let role = source.get_username();
    let database = source.get_database().unwrap();
    anyhow::ensure!(
        role.starts_with("migration_")
            && database.starts_with("native_audit_")
            && role.bytes().chain(database.bytes()).all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "unsafe fixture names"
    );
    sqlx::query(&format!("CREATE ROLE {role} LOGIN")).execute(&mut admin).await?;
    sqlx::query(&format!("CREATE DATABASE {database} OWNER {role}")).execute(&mut admin).await?;
    let wrong_url = format!("postgres://{role}@127.0.0.1:{}/{database}", options.get_port());
    let progress = MigrationReporter::new("wrong-server".into());
    let cloned = progress.clone();
    let source_url = db.url.clone();
    let (approval, receive) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        ManagedMigration::worker_guarded(&source_url, &cloned, Some(receive)).await
    });
    timeout(Duration::from_secs(5), async {
        while progress.backend.lock().unwrap().is_none() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let owner = progress.backend.lock().unwrap().clone().unwrap();
    let mut control = MigrationSession::connect(&db.url, "native-proven-observer").await?;
    assert!(MigrationSession::exists(&mut control, &owner).await?);
    let observer = MigrationSession::identity(&mut control).await?;
    assert_eq!(
        MigrationSession::signal(&mut control, &observer, true).await.unwrap_err().code(),
        "cancellation_unconfirmed"
    );
    control.ping().await?;
    // A reviewed new generation on a changed target must not filter away a
    // previously unconfirmed owner, even if its record came from another endpoint.
    let original_key = MigrationDurable::bootstrap(&mut control, "prior-orphan").await?;
    let mut foreign_key = original_key.clone();
    foreign_key.target.push_str(":old-endpoint");
    let prior = MigrationReporter::new("prior-orphan".into());
    *prior.backend.lock().unwrap() = Some(owner.clone());
    NativeDatabase::save(&mut control, &foreign_key, &prior).await?;
    let next = MigrationDurable::bootstrap(&mut control, "new-generation").await?;
    assert_eq!(
        MigrationDurable::owners(&mut control, &next).await.unwrap_err().code(),
        "state_corrupt"
    );
    assert!(MigrationSession::exists(&mut control, &owner).await?);
    control.close().await?;
    // Reconnect to a different actual writer with identical login/database names.
    let mut control = MigrationSession::connect(&wrong_url, "native-reconnected-observer").await?;
    let mut unrelated =
        MigrationSession::connect(&wrong_url, "native-wrong-server-unrelated").await?;
    assert_eq!(MigrationSession::identity(&mut control).await?.database, owner.database);
    assert_eq!(MigrationSession::identity(&mut control).await?.role, owner.role);
    assert_eq!(
        ManagedMigration::approve(&mut control, &progress, &worker).await.unwrap_err().code(),
        "cancellation_unconfirmed"
    );
    assert_eq!(
        MigrationSession::exists(&mut control, &owner).await.unwrap_err().code(),
        "cancellation_unconfirmed"
    );
    assert_eq!(
        MigrationSession::signal(&mut control, &owner, true).await.unwrap_err().code(),
        "cancellation_unconfirmed"
    );
    assert_eq!(
        MigrationSession::stop(&mut control, &owner, Instant::now(), Duration::from_secs(3))
            .await
            .unwrap_err()
            .code(),
        "cancellation_unconfirmed"
    );
    unrelated.ping().await?;
    let mut correct =
        MigrationSession::connect(&db.url, "native-original-owner-verification").await?;
    assert!(
        MigrationSession::exists(&mut correct, &owner).await?,
        "wrong writer did not signal the owner"
    );
    approval.send(false).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(5), worker).await??.unwrap_err().code(),
        "cancellation_unconfirmed"
    );
    let root: bool =
        sqlx::query_scalar("SELECT to_regclass('public.transaction_events') IS NOT NULL")
            .fetch_one(&mut correct)
            .await?;
    assert!(!root, "wrong-server preflight cannot authorize DDL");
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn delayed_auxiliary_running_writer_cannot_overwrite_terminal_failure() -> anyhow::Result<()>
{
    for baseline in [true, false] {
        let mut db = NativeDatabase::new().await?;
        PgTransactionEventSink::migrate(&db.url).await?;
        let mut blocker = MigrationSession::connect(&db.url, "native-late-ledger-blocker").await?;
        let held = NativeDatabase::held_writer(&mut blocker).await?;
        let mut control = MigrationSession::connect(&db.url, "native-late-ledger-observer").await?;
        MigrationDurable::bootstrap(&mut control, "late-writer").await?;
        sqlx::raw_sql("CREATE FUNCTION delay_auxiliary_record() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('application_name')='audit-migrate-observe' AND NEW.record->'backend'<>'null'::jsonb THEN PERFORM pg_advisory_xact_lock(44556677); END IF; RETURN NEW; END $$; CREATE TRIGGER delay_auxiliary_record BEFORE INSERT ON public.audit_migration_runs FOR EACH ROW EXECUTE FUNCTION delay_auxiliary_record()")
            .execute(&mut db.admin).await?;
        sqlx::query("SELECT pg_advisory_lock(44556677)").execute(&mut db.admin).await?;
        let scratch = NativeDatabase::scratch()?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let binary = PathBuf::from(env::var(if baseline {
            "TIPS_AUDIT_TEST_BASELINE_BINARY"
        } else {
            "TIPS_AUDIT_TEST_BINARY"
        })?);
        let proxy = if baseline {
            let proxy =
                NativeResponseProxy::start_inner(&db.url, b"never-drop-this-query", true).await?;
            db.url = proxy.url.clone();
            Some(proxy)
        } else {
            None
        };
        let mut child = db.child_binary(&binary, port, "late-writer", &scratch).await?;
        if baseline {
            db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='audit-migrate-observe' AND wait_event='advisory')").await?;
        } else {
            db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_progress_create_index p JOIN pg_stat_activity a ON a.pid=p.pid WHERE a.datname=current_database() AND a.application_name LIKE 'audit-migrate-%' AND p.phase LIKE 'waiting%')").await?;
        }
        fs::rename(scratch.join("state.json"), scratch.join("state-before-fault.json"))?;
        fs::create_dir(scratch.join("state.json"))?;
        let pid = child.id().unwrap();
        assert!(Command::new("kill").args(["-TERM", &pid.to_string()]).status().await?.success());
        timeout(Duration::from_secs(2), async {
            loop {
                let failed: bool = sqlx::query_scalar("SELECT record->'status'->>'state'='failed' FROM audit_migration_runs WHERE generation='late-writer'").fetch_one(&mut control).await?;
                if failed { return anyhow::Ok(()); }
                sleep(Duration::from_millis(5)).await;
            }
        }).await??;
        // Wait for BOTH terminal/control cache-failure commits before releasing
        // the old auxiliary statement; the child exit confirms no later control write.
        assert!(!timeout(Duration::from_secs(9), child.wait()).await??.success());
        sqlx::query("SELECT pg_advisory_unlock(44556677)").execute(&mut db.admin).await?;
        let state: String = timeout(Duration::from_secs(2), async {
            loop {
                let observed: String = sqlx::query_scalar("SELECT record->'status'->>'state' FROM audit_migration_runs WHERE generation='late-writer'").fetch_one(&mut control).await?;
                if !baseline || observed == "running" { return anyhow::Ok(observed); }
                sleep(Duration::from_millis(5)).await;
            }
        }).await??;
        drop(proxy);
        eprintln!(
            "ledger baseline={baseline}, failed_before_release=true, state_after_release={state}"
        );
        assert_eq!(
            state,
            if baseline { "running" } else { "failed" },
            "baseline reproduces the late auxiliary overwrite; corrected control has no auxiliary ledger writer"
        );
        if !baseline {
            let key = MigrationDurable::bootstrap(&mut control, "late-writer").await?;
            let terminal = MigrationDurable::load(&mut control, &key).await?.unwrap();
            let stale = MigrationReporter::new("late-writer".into());
            assert!(
                MigrationDurable::save(&mut control, &key, &stale).await.is_err(),
                "non-gate-owner cannot overwrite terminal"
            );
            assert_eq!(
                MigrationDurable::load(&mut control, &key).await?.unwrap().status.state,
                terminal.status.state
            );
            let replacement = NativeDatabase::scratch()?;
            let mut suppressed = db.child(port, "late-writer", &replacement).await?;
            assert_eq!(NativeDatabase::status(port, MigrationState::Failed).await?.attempt, 1);
            NativeDatabase::signal(&mut suppressed, "-INT").await?;
        }
        held.rollback().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn non_gate_owner_cache_or_network_failure_cannot_mutate_active_generation()
-> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut blocker = MigrationSession::connect(&db.url, "native-gate-first-blocker").await?;
    let held = NativeDatabase::held_writer(&mut blocker).await?;
    let first = MigrationReporter::new("shared-generation".into());
    let config = ManagedMigrationConfig {
        database_url: db.url.clone(),
        address: "127.0.0.1:0".parse()?,
        metrics_enabled: false,
        metrics_interval_secs: 1,
        run_id: "shared-generation".into(),
        state_path: NativeDatabase::scratch()?.join("state.json"),
        shutdown_timeout: Duration::from_secs(9),
    };
    let first_config = config.clone();
    let cloned = first.clone();
    let first_task =
        tokio::spawn(
            async move { ManagedMigration::supervise(&first_config, &cloned, None).await },
        );
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_progress_create_index p JOIN pg_stat_activity a ON a.pid=p.pid WHERE a.datname=current_database() AND a.application_name LIKE 'audit-migrate-%' AND p.phase LIKE 'waiting%')").await?;
    let owner = first.backend.lock().unwrap().clone().unwrap();
    let key = first.durable.lock().unwrap().clone().unwrap();
    let mut control = MigrationSession::connect(&db.url, "native-gate-record-verification").await?;
    let original: serde_json::Value = sqlx::query_scalar(
        "SELECT record FROM audit_migration_runs WHERE generation='shared-generation'",
    )
    .fetch_one(&mut control)
    .await?;
    for cache_failure in [true, false] {
        let mut second = MigrationReporter::new("shared-generation".into());
        *second.durable.lock().unwrap() = Some(key.clone());
        *second.backend.lock().unwrap() = Some(owner.clone());
        let proxy = NativeResponseProxy::start(&db.url, b"pg_try_advisory_lock").await?;
        let mut second_config = config.clone();
        if cache_failure {
            second.store = Some(MigrationStore {
                path: NativeDatabase::scratch()?.join("missing-parent/state.json"),
            });
        } else {
            second_config.database_url = proxy.url.clone();
        }
        let cloned = second.clone();
        let task = tokio::spawn(async move {
            ManagedMigration::supervise(&second_config, &cloned, None).await
        });
        timeout(Duration::from_secs(5), async {
            while second.snapshot().state != MigrationState::Failed {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        if !cache_failure {
            assert!(proxy.triggered.is_cancelled());
        }
        let current: serde_json::Value = sqlx::query_scalar(
            "SELECT record FROM audit_migration_runs WHERE generation='shared-generation'",
        )
        .fetch_one(&mut control)
        .await?;
        assert_eq!(current, original, "a copied cache key is not gate ownership");
        assert!(
            MigrationSession::exists(&mut control, &owner).await?,
            "unauthorized supervisor did not signal first worker"
        );
        second.cancel.cancel();
        let result = timeout(Duration::from_secs(3), task).await??;
        assert!(result.is_err());
    }
    first.cancel.cancel();
    timeout(Duration::from_secs(9), first_task).await???;
    assert!(!MigrationSession::exists(&mut control, &owner).await?);
    // A fresh gate owner reloads the now-committed STOPPED record rather than the
    // stale RUNNING cache. It can safely resume after the original owner is gone.
    held.rollback().await?;
    let resumed = MigrationReporter::new("shared-generation".into());
    *resumed.durable.lock().unwrap() = Some(key);
    *resumed.backend.lock().unwrap() = Some(owner);
    let cloned = resumed.clone();
    let task =
        tokio::spawn(async move { ManagedMigration::supervise(&config, &cloned, None).await });
    timeout(Duration::from_secs(15), async {
        while resumed.snapshot().state != MigrationState::Succeeded {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(resumed.snapshot().attempt, 2);
    resumed.cancel.cancel();
    timeout(Duration::from_secs(3), task).await???;
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn committed_early_failure_releases_generation_gate_before_idle() -> anyhow::Result<()> {
    let db = NativeDatabase::new().await?;
    let progress = MigrationReporter::new("gate-error-idle".into());
    let mut writes = 0;
    *progress.cache_writer.lock().unwrap() = Some(MigrationCacheWriter::with_sink(move |_, _| {
        writes += 1;
        if writes == 1 { Ok(()) } else { Err(audit_archiver_lib::MigrationError::StateIo) }
    })?);
    let config = ManagedMigrationConfig {
        database_url: db.url.clone(),
        address: "127.0.0.1:0".parse()?,
        metrics_enabled: false,
        metrics_interval_secs: 1,
        run_id: "gate-error-idle".into(),
        state_path: NativeDatabase::scratch()?.join("state.json"),
        shutdown_timeout: Duration::from_secs(9),
    };
    let cloned = progress.clone();
    let task =
        tokio::spawn(async move { ManagedMigration::supervise(&config, &cloned, None).await });
    let mut control = MigrationSession::connect(&db.url, "native-gate-error-verifier").await?;
    timeout(Duration::from_secs(5), async {
        loop {
            let key = progress.durable.lock().unwrap().clone();
            if let Some(key) = key
                && MigrationDurable::load(&mut control, &key)
                    .await?
                    .is_some_and(|record| record.status.state == MigrationState::Failed)
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    timeout(Duration::from_secs(3), async {
        loop {
            let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(MigrationDurable::GATE)
                .fetch_one(&mut control)
                .await?;
            if acquired {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    assert!(!task.is_finished(), "failed result is still hosted after releasing gate");
    MigrationDurable::unlock(&mut control).await?;
    progress.cancel.cancel();
    assert!(timeout(Duration::from_secs(3), task).await??.is_err());
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn missing_runtime_insert_column_cannot_restore_schema_readiness() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    AuditMigration::run(&db.url).await?;
    let history: Vec<(i64, bool, Vec<u8>)> =
        sqlx::query_as("SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut db.admin)
            .await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let initial = NativeDatabase::scratch()?;
    let mut succeeded = db.child(port, "missing-runtime-column", &initial).await?;
    assert!(NativeDatabase::status(port, MigrationState::Succeeded).await?.complete);
    NativeDatabase::signal(&mut succeeded, "-INT").await?;
    sqlx::query("ALTER TABLE public.transaction_events DROP COLUMN request_id")
        .execute(&mut db.admin)
        .await?;
    TransactionEventIngestedAtIndex::validate(&mut db.admin).await?;
    assert!(AuditMigration::verify_schema(&mut db.admin).await.is_err());
    assert!(AuditMigration::run(&db.url).await.is_err());
    let sink = PgTransactionEventSink::connect(&db.url, 1).await?;
    assert!(sink.check_schema_ready().await.is_err());
    for _ in 0..2 {
        let scratch = NativeDatabase::scratch()?;
        let mut child = db.child(port, "missing-runtime-column", &scratch).await?;
        let failed = NativeDatabase::status(port, MigrationState::Failed).await?;
        assert!(!failed.schema_ready && !failed.ready && !failed.complete);
        assert_eq!(failed.attempt, 1);
        assert_eq!(NativeDatabase::http(port, "/healthz").await?.0, 200);
        assert_eq!(NativeDatabase::http(port, "/readyz").await?.0, 503);
        NativeDatabase::signal(&mut child, "-TERM").await?;
    }
    let after: Vec<(i64, bool, Vec<u8>)> =
        sqlx::query_as("SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut db.admin)
            .await?;
    assert_eq!(history, after);
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17 and locally built audit binary"]
async fn missing_runtime_table_never_restores_schema_readiness() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    AuditMigration::run(&db.url).await?;
    sqlx::query("DROP TABLE public.transaction_events CASCADE").execute(&mut db.admin).await?;
    assert!(AuditMigration::run(&db.url).await.is_err());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    for restored in [false, true] {
        let scratch = NativeDatabase::scratch()?;
        let mut child = db.child(port, "missing-root", &scratch).await?;
        let failed = NativeDatabase::status(port, MigrationState::Failed).await?;
        assert!(!failed.schema_ready && !failed.ready && !failed.complete);
        assert_eq!(failed.attempt, 1);
        assert_eq!(NativeDatabase::http(port, "/healthz").await?.0, 200);
        assert_eq!(NativeDatabase::http(port, "/readyz").await?.0, 503);
        NativeDatabase::signal(&mut child, if restored { "-INT" } else { "-TERM" }).await?;
    }
    let progress = MigrationReporter::new("missing-root-stopped".into());
    {
        let mut status = progress.status.lock().unwrap();
        status.state = MigrationState::Stopped;
        status.cleanup_confirmed = true;
        status.cancellation_confirmed = true;
    }
    let mut control = MigrationSession::connect(&db.url, "native-missing-root-restore").await?;
    let key = MigrationDurable::bootstrap(&mut control, "missing-root-stopped").await?;
    NativeDatabase::save(&mut control, &key, &progress).await?;
    let scratch = NativeDatabase::scratch()?;
    let mut child = db.child(port, "missing-root-stopped", &scratch).await?;
    let failed = NativeDatabase::status(port, MigrationState::Failed).await?;
    assert_eq!(failed.attempt, 2);
    assert!(!failed.schema_ready && !failed.ready && !failed.complete);
    assert_eq!(NativeDatabase::http(port, "/healthz").await?.0, 200);
    assert_eq!(NativeDatabase::http(port, "/readyz").await?.0, 503);
    NativeDatabase::signal(&mut child, "-TERM").await?;
    let root: bool =
        sqlx::query_scalar("SELECT to_regclass('public.transaction_events') IS NOT NULL")
            .fetch_one(&mut control)
            .await?;
    assert!(!root, "restoration cannot recreate dropped applied schema");
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn compatibility_index_rejects_checksum_drift_before_any_catalog_mutation()
-> anyhow::Result<()> {
    for needs_build in [false, true] {
        let mut db = NativeDatabase::new().await?;
        if needs_build {
            PgTransactionEventSink::migrate(&db.url).await?;
        } else {
            AuditMigration::run(&db.url).await?;
        }
        sqlx::query(
            "UPDATE public._sqlx_migrations SET checksum=decode('00','hex') WHERE version=2",
        )
        .execute(&mut db.admin)
        .await?;
        let catalog_sql = "SELECT c.oid::bigint,c.relname::text,i.indisvalid,i.indisready,i.indrelid::bigint FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relnamespace='public'::regnamespace ORDER BY c.oid";
        let history_sql =
            "SELECT version,success,checksum FROM public._sqlx_migrations ORDER BY version";
        let catalog: Vec<(i64, String, bool, bool, i64)> =
            sqlx::query_as(catalog_sql).fetch_all(&mut db.admin).await?;
        let history: Vec<(i64, bool, Vec<u8>)> =
            sqlx::query_as(history_sql).fetch_all(&mut db.admin).await?;
        assert!(index_transaction_event_partitions(&db.url).await.is_err());
        assert!(AuditMigration::run(&db.url).await.is_err());
        let after_catalog: Vec<(i64, String, bool, bool, i64)> =
            sqlx::query_as(catalog_sql).fetch_all(&mut db.admin).await?;
        let after_history: Vec<(i64, bool, Vec<u8>)> =
            sqlx::query_as(history_sql).fetch_all(&mut db.admin).await?;
        assert_eq!(catalog, after_catalog);
        assert_eq!(history, after_history);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn stalled_cache_writer_keeps_probes_live_and_does_not_delay_owned_cleanup()
-> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    PgTransactionEventSink::migrate(&db.url).await?;
    let mut blocker = MigrationSession::connect(&db.url, "native-stalled-cache-blocker").await?;
    let held = NativeDatabase::held_writer(&mut blocker).await?;
    let progress = MigrationReporter::new("stalled-cache".into());
    let cloned = progress.clone();
    let url = db.url.clone();
    let mut worker = tokio::spawn(async move { ManagedMigration::worker(&url, &cloned).await });
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_progress_create_index p JOIN pg_stat_activity a ON a.pid=p.pid WHERE a.datname=current_database() AND a.application_name LIKE 'audit-migrate-%' AND p.phase LIKE 'waiting%')").await?;
    let owner = progress.backend.lock().unwrap().clone().unwrap();
    let scratch = NativeDatabase::scratch()?;
    let store = MigrationStore { path: scratch.join("state.json") };
    let sink = store.clone();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let mut entered = Some(entered);
    let (release, barrier) = mpsc::channel();
    *progress.cache_writer.lock().unwrap() =
        Some(MigrationCacheWriter::with_sink(move |record, revoked| {
            if let Some(entered) = entered.take() {
                let _ = entered.send(());
                let _ = barrier.recv();
            }
            sink.save_record(&record, || !revoked.load(Ordering::Acquire))
        })?);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let router = MigrationHttp { progress: progress.clone(), metrics: None }.router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let mut control = MigrationSession::connect(&db.url, "native-stalled-cache-control").await?;
    let cloned = progress.clone();
    let started = Instant::now();
    let stopping = tokio::spawn(async move {
        let result =
            ManagedMigration::shutdown(&mut control, &mut worker, &cloned, Duration::from_secs(9))
                .await;
        (result, control)
    });
    timeout(Duration::from_secs(2), waiting).await??;
    for path in ["/healthz", "/readyz", "/status"] {
        let response =
            timeout(Duration::from_millis(500), NativeDatabase::http(port, path)).await??;
        assert_eq!(response.0, if path == "/readyz" { 503 } else { 200 });
    }
    let (result, mut control) = timeout(Duration::from_secs(9), stopping).await??;
    assert_eq!(result.unwrap_err().code(), "state_io");
    assert!(started.elapsed() < Duration::from_secs(9));
    assert!(!MigrationSession::exists(&mut control, &owner).await?);
    assert!(progress.snapshot().cleanup_confirmed);
    assert!(!progress.snapshot().ready && !progress.snapshot().complete);
    assert!(!store.path.exists(), "stalled write has not published a file");
    // The IO thread is still stuck here. Cleanup and probe responsiveness did not
    // require releasing it. A revoked old write must not later overwrite cleanup.
    let cloned = progress.clone();
    let terminal = tokio::spawn(async move {
        cloned
            .update(|s| {
                s.state = MigrationState::Failed;
                s.error_code = Some("state_io".into());
                s.cleanup_confirmed = true;
            })
            .await
    });
    tokio::task::yield_now().await;
    release.send(())?;
    timeout(Duration::from_secs(2), terminal).await???;
    let restored = store.load("stalled-cache")?.unwrap();
    assert_eq!(restored.status.state, MigrationState::Failed);
    assert!(restored.status.cleanup_confirmed && !restored.status.complete);
    let absent: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM pg_stat_progress_create_index WHERE pid=$1) AND NOT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1)")
        .bind(owner.pid).fetch_one(&mut control).await?;
    assert!(absent);
    server.abort();
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
    NativeDatabase::save(&mut store_conn, &key, &progress).await?;
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
async fn cold_managed_bootstrap_overlaps_ordinary_003_without_deadlock() -> anyhow::Result<()> {
    let mut db = NativeDatabase::new().await?;
    // This event trigger is confined to this unique owned fixture database.
    // Pause the ordinary 003 CREATE before relation creation, not its lock wait.
    sqlx::raw_sql("CREATE FUNCTION pause_ordinary_native_tables() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('application_name')='audit-migrate-up' AND strpos(current_query(),'audit_migration_identity')>0 THEN PERFORM pg_advisory_xact_lock(55998866); END IF; END $$; CREATE EVENT TRIGGER pause_ordinary_native_tables ON ddl_command_start WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION pause_ordinary_native_tables()")
        .execute(&mut db.admin).await?;
    sqlx::query("SELECT pg_advisory_lock(55998866)").execute(&mut db.admin).await?;
    let url = db.url.clone();
    let ordinary = tokio::spawn(async move { AuditMigration::run(&url).await });
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='audit-migrate-up' AND wait_event='advisory' AND query LIKE '%audit_migration_identity%')").await?;
    let absent: bool =
        sqlx::query_scalar("SELECT to_regclass('public.audit_migration_identity') IS NULL")
            .fetch_one(&mut db.admin)
            .await?;
    assert!(absent, "ordinary 003 is paused before creating native tables");
    let progress = MigrationReporter::new("cold-bootstrap".into());
    let config = ManagedMigrationConfig {
        database_url: db.url.clone(),
        address: "127.0.0.1:0".parse()?,
        metrics_enabled: false,
        metrics_interval_secs: 1,
        run_id: "cold-bootstrap".into(),
        state_path: NativeDatabase::scratch()?.join("state.json"),
        shutdown_timeout: Duration::from_secs(9),
    };
    let cloned = progress.clone();
    let managed =
        tokio::spawn(async move { ManagedMigration::supervise(&config, &cloned, None).await });
    timeout(Duration::from_secs(10), async {
        while progress.snapshot().phase != MigrationPhase::WaitingForLock
            || progress.backend.lock().unwrap().is_none()
        {
            sleep(Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    let native_present: bool = sqlx::query_scalar("SELECT to_regclass('public.audit_migration_identity') IS NOT NULL AND to_regclass('public.audit_migration_runs') IS NOT NULL").fetch_one(&mut db.admin).await?;
    assert!(native_present, "managed bootstrap committed while ordinary 003 was paused");
    assert!(!ordinary.is_finished() && !managed.is_finished());
    sqlx::query("SELECT pg_advisory_unlock(55998866)").execute(&mut db.admin).await?;
    timeout(Duration::from_secs(20), ordinary).await???;
    timeout(Duration::from_secs(20), async {
        while progress.snapshot().state != MigrationState::Succeeded {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    AuditMigration::verify_complete(&mut db.admin).await?;
    assert!(progress.snapshot().complete && progress.snapshot().cleanup_confirmed);
    progress.cancel.cancel();
    timeout(Duration::from_secs(3), managed).await???;
    sqlx::raw_sql("DROP EVENT TRIGGER pause_ordinary_native_tables; DROP FUNCTION pause_ordinary_native_tables()")
        .execute(&mut db.admin).await?;
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
    db.wait("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='audit-migrate-up' AND query LIKE '%pg_try_advisory_lock%')").await?;
    let url = db.url.clone();
    let schema = tokio::spawn(async move { PgTransactionEventSink::migrate(&url).await });
    let url = db.url.clone();
    let index = tokio::spawn(async move { index_transaction_event_partitions(&url).await });
    db.wait("SELECT count(*)>=3 FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND query LIKE '%pg_try_advisory_lock%'").await?;
    assert!(!schema.is_finished() && !index.is_finished());
    assert!(!attempt.is_finished());
    sqlx::migrate::Migrate::unlock(&mut owner).await?;
    timeout(Duration::from_secs(20), attempt).await???;
    timeout(Duration::from_secs(20), schema).await???;
    timeout(Duration::from_secs(20), index).await???;
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
        NativeDatabase::save(&mut durable, &key, &seed).await?;
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
            while !progress.snapshot().worker_available || progress.snapshot().state != state {
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
    progress.finish(Err(audit_archiver_lib::MigrationError::CancellationUnconfirmed)).await?;
    NativeDatabase::save(&mut store_conn, &key, &progress).await?;
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
#[ignore = "requires owned disposable PostgreSQL 17"]
async fn new_generation_persists_prior_owner_cleanup_without_hiding_failed_record()
-> anyhow::Result<()> {
    let db = NativeDatabase::new().await?;
    let mut old = MigrationSession::connect(&db.url, "native-prior-failed-owner").await?;
    let backend = MigrationSession::identity(&mut old).await?;
    let mut control = MigrationSession::connect(&db.url, "native-prior-failed-seed").await?;
    let old_key = MigrationDurable::bootstrap(&mut control, "failed-before-bump").await?;
    let prior = MigrationReporter::new("failed-before-bump".into());
    *prior.backend.lock().unwrap() = Some(backend.clone());
    prior.finish(Err(audit_archiver_lib::MigrationError::CancellationUnconfirmed)).await?;
    NativeDatabase::save(&mut control, &old_key, &prior).await?;
    let progress = MigrationReporter::new("reviewed-bump".into());
    let config = ManagedMigrationConfig {
        database_url: db.url.clone(),
        address: "127.0.0.1:0".parse()?,
        metrics_enabled: false,
        metrics_interval_secs: 1,
        run_id: "reviewed-bump".into(),
        state_path: NativeDatabase::scratch()?.join("state.json"),
        shutdown_timeout: Duration::from_secs(9),
    };
    let cloned = progress.clone();
    let task =
        tokio::spawn(async move { ManagedMigration::supervise(&config, &cloned, None).await });
    timeout(Duration::from_secs(15), async {
        while progress.snapshot().state != MigrationState::Succeeded {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let persisted = MigrationDurable::load(&mut control, &old_key).await?.unwrap();
    assert_eq!(persisted.status.state, MigrationState::Failed);
    assert!(persisted.status.cleanup_confirmed);
    assert_eq!(persisted.backend.as_ref(), Some(&backend));
    assert!(!MigrationSession::exists(&mut control, &backend).await?);
    assert!(old.ping().await.is_err());
    let next = MigrationDurable::bootstrap(&mut control, "later-bump").await?;
    assert!(MigrationDurable::owners(&mut control, &next).await?.is_empty());
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
    NativeDatabase::save(&mut conn, &key, &progress).await?;
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
