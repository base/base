//! Subprocess regressions for Base snapshot defaults and metadata-only planning.
//!
//! Live discovery cases are ignored by default; run them with `just check-snapshot-manifests`.

use std::{
    process::{Command, Output},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    http::{StatusCode, Uri},
};
use rstest::rstest;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{net::TcpListener, process::Command as AsyncCommand, time::timeout};

/// Runs the unified binary's metadata-only download path in an isolated home.
#[derive(Debug)]
pub struct SnapshotCli;

impl SnapshotCli {
    /// Discovers or loads a manifest without creating a snapshot datadir.
    pub async fn plan(chain: &str, manifest_url: Option<&str>) -> Output {
        let home = TempDir::new().unwrap();
        let datadir = home.path().join("data");
        let mut command = AsyncCommand::new(env!("CARGO_BIN_EXE_base"));
        command
            .args([
                "--logs.stdout.quiet",
                "snapshot",
                "download",
                "--chain",
                chain,
                "--non-interactive",
                "--print-plan-json",
                "--datadir",
            ])
            .arg(&datadir)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("NO_COLOR", "1")
            .env_remove("BASE_CHAIN")
            .env_remove("BASE_NODE_LOG_DIR")
            .env_remove("BASE_NODE_METRICS_ENABLED")
            .kill_on_drop(true);
        if let Some(url) = manifest_url {
            command.args(["--manifest-url", url]);
        }

        let output = timeout(Duration::from_secs(60), command.output())
            .await
            .expect("snapshot planning exceeded 60 seconds")
            .expect("failed to run base snapshot download");
        assert!(!datadir.exists(), "metadata-only planning created the snapshot datadir");
        output
    }
}

#[test]
fn snapshot_download_help_advertises_base_defaults() {
    let isolated_home = TempDir::new().expect("failed to create isolated home directory");

    let output = Command::new(env!("CARGO_BIN_EXE_base"))
        .args(["snapshot", "download", "--help"])
        .env("NO_COLOR", "1")
        .env("HOME", isolated_home.path())
        .env("XDG_CONFIG_HOME", isolated_home.path())
        .env_remove("BASE_CHAIN")
        .output()
        .expect("failed to run base snapshot download --help");

    assert!(
        output.status.success(),
        "snapshot download help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let help = String::from_utf8(output.stdout).expect("snapshot download help must be UTF-8");
    let help = help.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "Browse available snapshots at https://chain.base.org",
        "https://mainnet-v2-snapshots.base.org (--chain mainnet)",
        "https://sepolia-v2-snapshots.base.org (--chain sepolia)",
        "https://zeronet-v2-snapshots.base.org (--chain zeronet)",
    ] {
        assert!(help.contains(expected), "snapshot download help is missing {expected:?}");
    }
    for legacy_source in ["snapshots.reth.rs", "publicnode.com/snapshots"] {
        assert!(
            !help.contains(legacy_source),
            "snapshot download help still advertises legacy source {legacy_source:?}"
        );
    }
}

#[rstest]
#[case::valid(StatusCode::OK, false, None)]
#[case::invalid_schema(StatusCode::OK, true, Some("storage_version"))]
#[case::http_error(StatusCode::NOT_FOUND, false, Some("404"))]
#[tokio::test]
async fn snapshot_plan_fetches_only_manifest(
    #[case] status: StatusCode,
    #[case] omit_storage_version: bool,
    #[case] expected_error: Option<&str>,
) {
    let mut manifest = json!({
        "chain_id": 763360,
        "block": 1234,
        "storage_version": 2,
        "timestamp": 1700000000,
        "components": {
            "state": {
                "file": "state.tar.zst",
                "size": 73,
                "output_files": [{
                    "path": "mdbx.dat",
                    "size": 211,
                    "blake3": "0000000000000000000000000000000000000000000000000000000000000000"
                }]
            }
        }
    });
    if omit_storage_version {
        manifest.as_object_mut().unwrap().remove("storage_version");
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let request_log = Arc::clone(&requests);
    let app = Router::new().fallback(move |uri: Uri| {
        let requests = Arc::clone(&request_log);
        let body = manifest.to_string();
        async move {
            requests.lock().unwrap().push(uri.path().to_owned());
            if uri.path() == "/manifest.json" {
                (status, body)
            } else {
                (StatusCode::NOT_FOUND, "archive requests are forbidden".to_owned())
            }
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let output =
        SnapshotCli::plan("zeronet", Some(&format!("http://{address}/manifest.json"))).await;
    server.abort();
    assert_eq!(*requests.lock().unwrap(), ["/manifest.json"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Some(error) = expected_error {
        assert!(!output.status.success(), "invalid manifest unexpectedly succeeded");
        assert!(stderr.contains(error), "expected {error:?} in {stderr}");
    } else {
        assert!(output.status.success(), "snapshot planning failed: {stderr}");
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["chainId"], 763360);
        assert_eq!(plan["block"], 1234);
        assert_eq!(plan["totalDownloadSize"], 73);
        assert_eq!(plan["totalOutputSize"], 211);
        let archives = plan["archives"].as_array().unwrap();
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0]["url"], format!("http://{address}/state.tar.zst"));
    }
}

#[rstest]
#[case::mainnet("mainnet", 8453)]
#[case::sepolia("sepolia", 84532)]
#[case::zeronet("zeronet", 763360)]
#[ignore = "queries production snapshot metadata; run with just check-snapshot-manifests"]
#[tokio::test]
async fn live_snapshot_plan(#[case] chain: &str, #[case] expected_id: u64) {
    let output = SnapshotCli::plan(chain, None).await;
    assert!(
        output.status.success(),
        "{chain} snapshot discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["chainId"], expected_id);
    let archives = plan["archives"].as_array().unwrap();
    assert!(!archives.is_empty(), "{chain} snapshot plan is empty");
    println!("{chain}: block {}, {} planned archives", plan["block"], archives.len());
}
