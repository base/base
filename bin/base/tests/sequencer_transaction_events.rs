//! Subprocess coverage of the unified sequencer's builder journal startup contract.

#[cfg(unix)]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};
use std::{process::Stdio, time::Duration};

use tempfile::TempDir;
use tokio::{
    process::Command,
    time::{sleep, timeout},
};

/// Constructs an isolated sequencer without requiring reachable L1 services.
#[derive(Debug)]
pub struct SequencerCli;

impl SequencerCli {
    /// Starts with builder journal settings supplied through deployment environment variables.
    pub fn command(home: &TempDir) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_base"));
        // Avoid inherited deployment settings changing the startup path under test.
        for (key, _) in std::env::vars_os() {
            let name = key.as_os_str().as_encoded_bytes();
            if name.starts_with(b"BASE_")
                || name.starts_with(b"BUILDER_")
                || name.starts_with(b"OTEL_")
            {
                command.env_remove(key);
            }
        }
        command
            .args([
                "--chain",
                "sepolia",
                "sequencer",
                "--l1-eth-rpc",
                "http://127.0.0.1:1",
                "--l1-beacon",
                "http://127.0.0.1:1",
                "--p2p.sequencer.key",
                "bcc617ea05150ff60490d3c6058630ba94ae9f12a02a87efd291349ca0e54e0a",
                "--datadir",
            ])
            .arg(home.path().join("data"))
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("BUILDER_TRANSACTION_EVENTS_ENABLED", "true")
            .env("BUILDER_TRANSACTION_EVENTS_REQUIRED", "true")
            .env("BUILDER_TRANSACTION_EVENTS_NETWORK", "sequencer-test")
            .env("BUILDER_TRANSACTION_EVENTS_PATH", home.path().join("journal/events.jsonl"))
            .kill_on_drop(true);
        command
    }
}

#[tokio::test]
async fn sequencer_startup_creates_builder_journal() {
    let home = TempDir::new().unwrap();
    let journal = home.path().join("journal/events.jsonl");
    let mut child =
        SequencerCli::command(&home).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let observed = timeout(Duration::from_secs(15), async {
        loop {
            if journal.is_file() {
                return true;
            }
            if child.try_wait().unwrap().is_some() {
                return false;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    // Always reap the subprocess, including on a missing-writer regression.
    child.kill().await.unwrap();
    assert!(observed.unwrap_or(false), "unified startup did not create the builder journal");
}

#[tokio::test]
async fn sequencer_required_builder_journal_open_failure_aborts_startup() {
    let home = TempDir::new().unwrap();
    // An existing directory cannot be opened as the journal file, even as root.
    let output = timeout(
        Duration::from_secs(15),
        SequencerCli::command(&home).env("BUILDER_TRANSACTION_EVENTS_PATH", home.path()).output(),
    )
    .await
    .expect("startup did not reject an unwritable required journal")
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to initialize required transaction event writer"), "{stderr}");
    assert!(stderr.contains(home.path().to_str().unwrap()), "{stderr}");
}

#[cfg(unix)]
#[tokio::test]
async fn sequencer_startup_tolerates_non_unicode_inherited_environment() {
    let output = timeout(
        Duration::from_secs(30),
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sequencer_required_builder_journal_open_failure_aborts_startup",
                "--nocapture",
            ])
            .env(OsString::from_vec(b"UNRELATED_\xff".to_vec()), "value")
            .env("UNRELATED_NON_UNICODE_VALUE", OsString::from_vec(vec![0xff]))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("non-Unicode environment regression exceeded its deadline")
    .unwrap();
    assert!(
        output.status.success(),
        "startup failed under non-Unicode environment:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
