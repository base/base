//! Opt-in qualification of the pinned real Reth/Lighthouse fork transition.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use alloy_consensus::Header;
use base_system_tests::{
    GlamsterdamClients, GlamsterdamConfig, GlamsterdamSchedule, L1Stack, L1StackConfig,
    RethContainer, SetupContainer,
};
use eyre::{Result, WrapErr, ensure};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

/// Wire observations for the real L1 qualification, not a simulated Engine driver.
#[derive(Debug)]
pub struct Qualification {
    /// Bounded HTTP client used for both EL and CL observations.
    pub http: reqwest::Client,
    /// Public EL JSON-RPC endpoint.
    pub el: String,
    /// Public CL beacon API endpoint.
    pub cl: String,
}

impl Qualification {
    /// Fetches an EL block without dropping Amsterdam-specific JSON fields.
    pub async fn block(&self, number: &str) -> Result<Value> {
        let response: Value = self
            .http
            .post(&self.el)
            .json(&json!({
                "jsonrpc":"2.0", "id":1, "method":"eth_getBlockByNumber", "params":[number, false]
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(response.get("error").is_none(), "EL block request failed: {response}");
        Ok(response["result"].clone())
    }

    /// Fetches a beacon API response, rejecting optimistic observations in the caller.
    pub async fn beacon(&self, path: &str) -> Result<Value> {
        Ok(self
            .http
            .get(format!("{}{path}", self.cl))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Reads a required JSON-RPC hex quantity.
    pub fn quantity(value: &Value) -> Result<u64> {
        let encoded = value.as_str().ok_or_else(|| eyre::eyre!("missing quantity: {value}"))?;
        Ok(u64::from_str_radix(encoded.trim_start_matches("0x"), 16)?)
    }

    /// Checks real block production, authenticated boundary linkage, and post-fork finality.
    pub async fn observe(
        &self,
        schedule: &GlamsterdamSchedule,
        origin: &str,
        artifacts: &Path,
    ) -> Result<()> {
        schedule.ensure_pre_fork()?;
        let spec = self.beacon("/eth/v1/config/spec").await?;
        ensure!(spec["data"]["PRESET_BASE"] == "minimal", "unexpected CL preset");
        ensure!(
            spec["data"]["SLOTS_PER_EPOCH"].as_str().and_then(|value| value.parse().ok())
                == Some(schedule.slots_per_epoch),
            "CL epoch length mismatch"
        );
        ensure!(
            spec["data"]["SECONDS_PER_SLOT"].as_str().and_then(|value| value.parse().ok())
                == Some(schedule.slot_duration),
            "CL slot duration mismatch"
        );
        ensure!(
            spec["data"]["GLOAS_FORK_EPOCH"].as_str().and_then(|value| value.parse().ok())
                == Some(schedule.activation_epoch),
            "CL activation epoch mismatch"
        );
        let genesis = self.block("0x0").await?;
        ensure!(
            genesis["hash"] == origin,
            "future L1 scheduling changed the generated genesis hash"
        );
        std::fs::write(artifacts.join("spec.json"), serde_json::to_vec_pretty(&spec)?)?;
        std::fs::write(
            artifacts.join("genesis-header.json"),
            serde_json::to_vec_pretty(&genesis)?,
        )?;

        let deadline = Instant::now() + Duration::from_secs(300);
        let mut boundary = None;
        loop {
            let head = self.block("latest").await?;
            let finality = self.beacon("/eth/v1/beacon/states/head/finality_checkpoints").await?;
            let fork = self.beacon("/eth/v1/beacon/states/head/fork").await?;
            let finalized = self.block("finalized").await?;
            let last =
                json!({"head": head, "fork": fork, "finality": finality, "finalized": finalized});
            std::fs::write(
                artifacts.join("last-observed.json"),
                serde_json::to_vec_pretty(&last)?,
            )?;
            if boundary.is_none()
                && Self::quantity(&head["timestamp"])? >= schedule.activation_timestamp
            {
                let mut post = head.clone();
                loop {
                    let number = Self::quantity(&post["number"])?;
                    ensure!(number > 0, "Amsterdam was active at genesis");
                    let pre = self.block(&format!("0x{:x}", number - 1)).await?;
                    if Self::quantity(&pre["timestamp"])? < schedule.activation_timestamp {
                        let pre_header: Header = serde_json::from_value(pre.clone())?;
                        let post_header: Header = serde_json::from_value(post.clone())?;
                        ensure!(
                            json!(pre_header.hash_slow()) == pre["hash"],
                            "pre-fork header authentication failed"
                        );
                        ensure!(
                            json!(post_header.hash_slow()) == post["hash"],
                            "post-fork header authentication failed"
                        );
                        ensure!(post["parentHash"] == pre["hash"], "boundary linkage mismatch");
                        ensure!(
                            pre["blockAccessListHash"].is_null() && pre["slotNumber"].is_null(),
                            "Amsterdam fields before fork"
                        );
                        ensure!(
                            post["blockAccessListHash"].is_string()
                                && post["slotNumber"].is_string(),
                            "missing Amsterdam fields after fork"
                        );
                        ensure!(
                            Self::quantity(&post["slotNumber"])?
                                == (Self::quantity(&post["timestamp"])?
                                    - schedule.genesis_timestamp)
                                    / schedule.slot_duration,
                            "EL slot/CL schedule mismatch"
                        );
                        boundary = Some(json!({"pre": pre, "post": post}));
                        std::fs::write(
                            artifacts.join("boundary.json"),
                            serde_json::to_vec_pretty(&boundary)?,
                        )?;
                        break;
                    }
                    post = pre;
                }
            }
            let finalized_epoch = finality["data"]["finalized"]["epoch"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing finalized epoch"))?
                .parse::<u64>()?;
            if boundary.is_some()
                && finalized_epoch > schedule.activation_epoch
                && !finalized.is_null()
                && Self::quantity(&finalized["timestamp"])? >= schedule.activation_timestamp
            {
                ensure!(
                    finality["execution_optimistic"] == false
                        && fork["execution_optimistic"] == false,
                    "optimistic CL observation is not qualification"
                );
                ensure!(fork["data"]["current_version"] == "0x80000000", "Gloas did not activate");
                let finalized_beacon = self.beacon("/eth/v2/beacon/blocks/finalized").await?;
                ensure!(
                    finalized_beacon["version"] == "gloas",
                    "finalized beacon block is not Gloas"
                );
                std::fs::write(
                    artifacts.join("finalized-beacon.json"),
                    serde_json::to_vec_pretty(&finalized_beacon)?,
                )?;
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "L1 qualification timed out; target activation={}, last={last}",
                schedule.activation_timestamp
            );
            sleep(Duration::from_secs(1)).await;
        }
    }
}

#[tokio::test]
#[ignore = "explicit client qualification; prebuild pinned images and set BASE_GLAMSTERDAM_ARTIFACTS"]
async fn reth_lighthouse_gloas_transition() -> Result<()> {
    let artifacts = PathBuf::from(
        std::env::var("BASE_GLAMSTERDAM_ARTIFACTS")
            .wrap_err("set BASE_GLAMSTERDAM_ARTIFACTS to a fresh evidence directory")?,
    );
    std::fs::create_dir_all(&artifacts)?;
    let output = artifacts.join("generated");
    ensure!(!output.exists(), "qualification requires fresh generated state");
    let clients = GlamsterdamClients::pinned()?;
    clients.require_local()?;
    let config = GlamsterdamConfig { activation_epoch: 8, slot_duration: 2 };
    let setup = SetupContainer::new(&output)
        .with_slot_duration(config.slot_duration)
        .with_glamsterdam_setup(clients.setup.clone());
    let (genesis, deployment) =
        tokio::task::spawn_blocking(move || setup.generate_genesis()).await??;
    let schedule = config.apply(&genesis)?;
    let rollup: Value = serde_json::from_str(&deployment.read_rollup_config()?)?;
    let origin =
        rollup["genesis"]["l1"]["hash"].as_str().ok_or_else(|| eyre::eyre!("missing L1 origin"))?;
    let container_config = clients.container_config(Some(&artifacts))?;
    let network = container_config.network_name.clone().unwrap();
    let stack = L1Stack::start(L1StackConfig {
        el_genesis_json: genesis.read_el_genesis()?,
        jwt_secret_hex: genesis.read_jwt_secret()?,
        testnet_dir: genesis.testnet_dir(),
        container_config: Some(container_config),
    })
    .await?;
    let observation = Qualification {
        http: reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?,
        el: stack.rpc_url().await?.to_string(),
        cl: stack.beacon_url().await?,
    };
    let result = observation.observe(&schedule, origin, &artifacts).await;
    let diagnostics = stack.capture_diagnostics(&artifacts).await;
    drop(stack);
    let network_exists =
        Command::new("docker").args(["network", "inspect", &network]).output()?.status.success();
    ensure!(!network_exists, "owned fixture network survived container cleanup: {network}");
    result?;
    diagnostics?;
    std::fs::write(
        artifacts.join("qualification.json"),
        serde_json::to_vec_pretty(
            &json!({"status":"passed", "clients": clients, "schedule": schedule}),
        )?,
    )?;
    Ok(())
}

#[tokio::test]
#[ignore = "explicit Docker failure-path qualification; requires the pinned Reth image"]
async fn reth_startup_failure_retains_logs_and_removes_network() -> Result<()> {
    let artifacts = tempfile::tempdir()?;
    let clients = GlamsterdamClients::pinned()?;
    let config = clients.container_config(Some(artifacts.path()))?;
    let network = config.network_name.clone().unwrap();
    let result =
        RethContainer::start("not valid genesis JSON", "11".repeat(32), Some(config)).await;
    ensure!(result.is_err(), "deliberately invalid genesis unexpectedly started");
    let log = std::fs::read_to_string(artifacts.path().join("reth.stream.log"))?;
    ensure!(!log.is_empty(), "startup failure lost client logs");
    let network_exists =
        Command::new("docker").args(["network", "inspect", &network]).output()?.status.success();
    ensure!(!network_exists, "failed startup leaked owned network: {network}");
    Ok(())
}
