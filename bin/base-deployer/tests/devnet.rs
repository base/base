//! Integration tests for the `base-deployer` binary.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[test]
#[ignore = "requires docker compose, image builds, and open local ports"]
fn starts_local_devnet_and_reports_status() -> Result<(), Box<dyn std::error::Error>> {
    let repo_root = repo_root();
    let binary = env!("CARGO_BIN_EXE_base-deployer");

    cleanup_devnet(&repo_root);

    let start = Command::new(binary).current_dir(&repo_root).arg("devnet").output()?;
    if !start.status.success() {
        return Err(format!(
            "base-deployer devnet failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&start.stdout),
            String::from_utf8_lossy(&start.stderr)
        )
        .into());
    }

    let status = Command::new(binary)
        .current_dir(&repo_root)
        .args(["status", "--json"])
        .output()?;

    cleanup_devnet(&repo_root);

    if !status.status.success() {
        return Err(format!(
            "base-deployer status failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        )
        .into());
    }

    let report: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert!(
        report["running_services"]
            .as_array()
            .is_some_and(|services| !services.is_empty()),
        "expected running services in status report"
    );
    assert_eq!(report["l1"]["reachable"].as_bool(), Some(true));
    assert_eq!(report["l2_builder"]["reachable"].as_bool(), Some(true));
    assert_eq!(report["l2_client"]["reachable"].as_bool(), Some(true));

    Ok(())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root should exist")
        .to_path_buf()
}

fn cleanup_devnet(repo_root: &Path) {
    let _ = Command::new("docker")
        .current_dir(repo_root)
        .args([
            "compose",
            "--env-file",
            "etc/docker/devnet-env",
            "-f",
            "etc/docker/docker-compose.yml",
            "down",
        ])
        .status();

    let devnet_dir = repo_root.join(".devnet");
    if devnet_dir.exists() {
        let _ = fs::remove_dir_all(devnet_dir);
    }
}
