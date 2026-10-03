//! Explicitly owned disposable PG17 fixtures; never connects to a foreign cluster.

use std::{
    env, fs,
    net::TcpListener,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use sqlx::{Connection, PgConnection};

/// One isolated cluster under this checkout's scratch directory.
/// Fixture credentials are disposable; this does not authorize external databases.
#[derive(Debug)]
pub struct OwnedPostgres {
    /// Unique loopback TCP port chosen before startup.
    pub port: u16,
    /// Disposable fixture URL.
    pub url: String,
    /// Canonical data directory owned by this test instance.
    pub directory: PathBuf,
    /// Local PG17 utilities supplied explicitly by the test runner.
    pub bin: PathBuf,
}

impl OwnedPostgres {
    /// Creates a private PG17 cluster only when the owned-binary opt-in is set.
    pub async fn new() -> Result<Self> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize()?;
        let bin = PathBuf::from(env::var("TIPS_AUDIT_TEST_PG_BIN")?).canonicalize()?;
        ensure!(bin.starts_with(root.join(".tmp")), "foreign PG utilities refused");
        let version = Command::new(bin.join("postgres")).arg("--version").output()?;
        ensure!(
            version.status.success() && String::from_utf8(version.stdout)?.contains(" 17."),
            "PG17 required"
        );
        let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let scratch = root.join(".tmp/2026-10-03-hourly-independent/tests").join(id.to_string());
        fs::create_dir_all(&scratch)?;
        let directory = scratch.join("cluster");
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let initialized = Command::new(bin.join("initdb"))
            .args(["-U", "postgres", "-A", "trust", "--no-sync"])
            .arg("-D")
            .arg(&directory)
            .output()?;
        ensure!(initialized.status.success(), "owned initdb failed");
        let fixture = Self {
            port,
            url: format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"),
            directory: directory.canonicalize()?,
            bin,
        };
        let started = Command::new(fixture.bin.join("pg_ctl"))
            .arg("-D")
            .arg(&fixture.directory)
            .arg("-l")
            .arg(scratch.join("server.log"))
            .arg("-o")
            .arg(format!(
                "-p {port} -h 127.0.0.1 -k '' -c shared_buffers=16MB -c max_connections=30"
            ))
            .args(["-w", "-t", "15", "start"])
            .output()?;
        ensure!(started.status.success(), "owned postgres start failed");
        // Never assume a released TCP port still refers to our process.
        let mut conn = PgConnection::connect(&fixture.url).await?;
        let (version, directory): (i32, String) = sqlx::query_as(
            "SELECT current_setting('server_version_num')::int,current_setting('data_directory')",
        )
        .fetch_one(&mut conn)
        .await?;
        ensure!(
            (170000..180000).contains(&version)
                && PathBuf::from(directory).canonicalize()? == fixture.directory,
            "foreign server refused"
        );
        conn.close().await?;
        Ok(fixture)
    }

    /// Dumps this private schema with the same filters as the Docker fixture.
    pub fn schema_dump(&self) -> Result<String> {
        let result = Command::new(self.bin.join("pg_dump"))
            .arg(&self.url)
            .args([
                "--schema-only",
                "--no-owner",
                "--exclude-table=_sqlx_migrations",
                "--exclude-table=transaction_events_*_2*",
            ])
            .output()?;
        ensure!(result.status.success(), "owned pg_dump failed");
        String::from_utf8(result.stdout).context("invalid schema dump encoding")
    }
}

impl Drop for OwnedPostgres {
    fn drop(&mut self) {
        // Exact private data directory, never an arbitrary PID or foreign socket.
        let _ = Command::new(self.bin.join("pg_ctl"))
            .arg("-D")
            .arg(&self.directory)
            .args(["-w", "-t", "15", "-m", "fast", "stop"])
            .output();
    }
}
