//! Docker Compose-based devnet orchestration and status checks.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use alloy_network::Ethereum;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use eyre::{Result, WrapErr, bail, ensure};
use serde::Serialize;
use tokio::time::{sleep, timeout};
use url::Url;

use crate::{config::DeployerConfig, deploy, devnet::role_accounts};

const DOCKER_COMPOSE_FILE: &str = "etc/docker/docker-compose.yml";
const DEVNET_ENV_FILE: &str = "etc/docker/devnet-env";
const DEVNET_DATA_DIR: &str = ".devnet";
const CONTAINER_START_TIMEOUT: Duration = Duration::from_secs(300);
const PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// Result of starting a devnet.
#[derive(Debug, Clone)]
pub(crate) enum DevnetStartResult {
    /// Local Docker Compose devnet is running.
    Local(StatusReport),
    /// External L1 artifacts were prepared successfully.
    External(ExternalArtifactsReport),
}

/// Summary of artifacts generated for an external L1.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExternalArtifactsReport {
    /// Path to the live deployment manifest.
    pub(crate) manifest_path: PathBuf,
    /// Path to the generated L2 genesis file.
    pub(crate) genesis_path: PathBuf,
    /// Path to the generated rollup config.
    pub(crate) rollup_path: PathBuf,
}

/// Summary of the local devnet runtime status.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusReport {
    /// Running Docker Compose services.
    pub(crate) running_services: Vec<String>,
    /// L1 RPC probe result.
    pub(crate) l1: RpcEndpointStatus,
    /// L2 builder RPC probe result.
    pub(crate) l2_builder: RpcEndpointStatus,
    /// L2 client RPC probe result.
    pub(crate) l2_client: RpcEndpointStatus,
}

/// Health and chain information for an RPC endpoint.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RpcEndpointStatus {
    /// Endpoint label.
    pub(crate) name: String,
    /// Endpoint URL.
    pub(crate) url: String,
    /// Whether the endpoint responded successfully.
    pub(crate) reachable: bool,
    /// Chain ID when the probe succeeded.
    pub(crate) chain_id: Option<u64>,
    /// Latest block number when the probe succeeded.
    pub(crate) block_number: Option<u64>,
    /// Human-readable error when the probe failed.
    pub(crate) error: Option<String>,
}

/// Starts a devnet locally or prepares artifacts for an external L1.
pub(crate) async fn start_devnet(
    config: DeployerConfig,
    l1_rpc: Option<&str>,
) -> Result<DevnetStartResult> {
    if let Some(l1_rpc) = l1_rpc {
        let l1 = deploy::deploy_l1(config.clone(), None, l1_rpc).await?;
        let l2 = deploy::deploy_l2(config, None, Some(l1_rpc)).await?;
        return Ok(DevnetStartResult::External(ExternalArtifactsReport {
            manifest_path: l1.manifest_path,
            genesis_path: l2.genesis_path,
            rollup_path: l2.rollup_path,
        }));
    }

    let repo_root = find_repo_root()?;
    let devnet_dir = repo_root.join(DEVNET_DATA_DIR);

    run_best_effort(
        Command::new("docker")
            .current_dir(&repo_root)
            .args(compose_args())
            .arg("down"),
    );
    reset_devnet_data_dir(&repo_root, &devnet_dir)?;

    run_checked(
        Command::new("docker")
            .current_dir(&repo_root)
            .args(compose_args())
            .arg("up")
            .arg("-d")
            .arg("--build")
            .arg("--scale")
            .arg("contender=0"),
        "start local devnet with Docker Compose",
    )?;

    let status = timeout(CONTAINER_START_TIMEOUT, async {
        loop {
            let status = collect_status().await?;
            if status.l1.reachable && status.l2_builder.reachable && status.l2_client.reachable {
                return Ok::<_, eyre::Error>(status);
            }
            sleep(PROBE_INTERVAL).await;
        }
    })
    .await
    .wrap_err("Timed out waiting for the local devnet to become healthy")??;

    Ok(DevnetStartResult::Local(status))
}

/// Collects the current local devnet status from Docker and the public RPC endpoints.
pub(crate) async fn collect_status() -> Result<StatusReport> {
    let repo_root = find_repo_root()?;
    let env = read_env_file(&repo_root.join(DEVNET_ENV_FILE))?;
    let running_services = running_services(&repo_root)?;

    Ok(StatusReport {
        running_services,
        l1: probe_rpc("l1", env_value(&env, "L1_RPC_URL")?).await,
        l2_builder: probe_rpc("l2_builder", env_value(&env, "L2_BUILDER_RPC_URL")?).await,
        l2_client: probe_rpc("l2_client", env_value(&env, "L2_CLIENT_RPC_URL")?).await,
    })
}

fn find_repo_root() -> Result<PathBuf> {
    let mut current = std::env::current_dir().wrap_err("Failed to read current directory")?;

    loop {
        if current.join(DOCKER_COMPOSE_FILE).exists() && current.join("Cargo.toml").exists() {
            return Ok(current);
        }
        if !current.pop() {
            break;
        }
    }

    bail!("Could not find repository root containing {DOCKER_COMPOSE_FILE}")
}

fn compose_args() -> [&'static str; 5] {
    ["compose", "--env-file", DEVNET_ENV_FILE, "-f", DOCKER_COMPOSE_FILE]
}

fn running_services(repo_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("docker")
        .current_dir(repo_root)
        .args(compose_args())
        .arg("ps")
        .arg("--services")
        .arg("--status")
        .arg("running")
        .output()
        .wrap_err("Failed to query Docker Compose status")?;

    ensure!(
        output.status.success(),
        "docker compose ps failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect())
}

fn reset_devnet_data_dir(repo_root: &Path, devnet_dir: &Path) -> Result<()> {
    ensure!(
        devnet_dir.starts_with(repo_root),
        "Refusing to clear {} because it is outside the repository root",
        devnet_dir.display()
    );

    if devnet_dir.exists() {
        fs::remove_dir_all(devnet_dir)
            .wrap_err_with(|| format!("Failed to remove {}", devnet_dir.display()))?;
    }
    fs::create_dir_all(devnet_dir)
        .wrap_err_with(|| format!("Failed to create {}", devnet_dir.display()))
}

fn run_best_effort(command: &mut Command) {
    let _ = command.status();
}

fn run_checked(command: &mut Command, purpose: &str) -> Result<()> {
    let output = command.output().wrap_err_with(|| format!("Failed to {purpose}"))?;
    ensure!(
        output.status.success(),
        "{purpose} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let contents =
        fs::read_to_string(path).wrap_err_with(|| format!("Failed to read {}", path.display()))?;
    let mut values = BTreeMap::new();

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = trimmed.split_once('=') {
            values.insert(key.trim().to_string(), value.trim().trim_matches('"').to_string());
        }
    }

    Ok(values)
}

fn env_value<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    env.get(key)
        .map(String::as_str)
        .ok_or_else(|| eyre::eyre!("Missing `{key}` in {DEVNET_ENV_FILE}"))
}

async fn probe_rpc(name: &str, url: &str) -> RpcEndpointStatus {
    let parsed = match url.parse::<Url>() {
        Ok(url) => url,
        Err(err) => {
            return RpcEndpointStatus {
                name: name.to_string(),
                url: url.to_string(),
                reachable: false,
                chain_id: None,
                block_number: None,
                error: Some(format!("invalid URL: {err}")),
            };
        }
    };

    let provider = RootProvider::<Ethereum>::new(RpcClient::builder().http(parsed));
    match provider.get_chain_id().await {
        Ok(chain_id) => match provider.get_block_number().await {
            Ok(block_number) => RpcEndpointStatus {
                name: name.to_string(),
                url: url.to_string(),
                reachable: true,
                chain_id: Some(chain_id),
                block_number: Some(block_number),
                error: None,
            },
            Err(err) => RpcEndpointStatus {
                name: name.to_string(),
                url: url.to_string(),
                reachable: false,
                chain_id: Some(chain_id),
                block_number: None,
                error: Some(err.to_string()),
            },
        },
        Err(err) => RpcEndpointStatus {
            name: name.to_string(),
            url: url.to_string(),
            reachable: false,
            chain_id: None,
            block_number: None,
            error: Some(err.to_string()),
        },
    }
}

/// Returns a short, human-readable summary of local devnet connection details.
pub(crate) fn format_local_report(report: &StatusReport) -> String {
    let roles = role_accounts();
    format!(
        "Services: {}\nL1 RPC: {} (chain {}, block {})\nL2 Builder RPC: {} (chain {}, block {})\nL2 Client RPC: {} (chain {}, block {})\nFunded deployer: {:#x}\nFunded sequencer: {:#x}",
        report.running_services.join(", "),
        report.l1.url,
        report.l1.chain_id.unwrap_or_default(),
        report.l1.block_number.unwrap_or_default(),
        report.l2_builder.url,
        report.l2_builder.chain_id.unwrap_or_default(),
        report.l2_builder.block_number.unwrap_or_default(),
        report.l2_client.url,
        report.l2_client.chain_id.unwrap_or_default(),
        report.l2_client.block_number.unwrap_or_default(),
        roles.deployer.address,
        roles.sequencer.address,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::read_env_file;

    #[test]
    fn parses_env_file() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let env_path = tempdir.path().join("devnet-env");
        fs::write(
            &env_path,
            "\
# comment
L1_RPC_URL=http://localhost:4545
L2_BUILDER_RPC_URL=http://localhost:7545
",
        )
        .expect("env file should be written");

        let values = read_env_file(&env_path).expect("env file should parse");
        assert_eq!(values["L1_RPC_URL"], "http://localhost:4545");
        assert_eq!(values["L2_BUILDER_RPC_URL"], "http://localhost:7545");
    }
}
