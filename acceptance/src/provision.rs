#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, process::Command};

use crate::{DevnetProfile, ScenarioConfig};

const RETH_GLAMSTERDAM_IMAGE: &str = "ghcr.io/paradigmxyz/reth@sha256:8ce703acf113b2a20705b6e76adebed20f74ba8591bc7c9407203b2968aca70d";
const LIGHTHOUSE_GLAMSTERDAM_IMAGE: &str =
    "sigp/lighthouse@sha256:c0a6cf874d0a0596bf6c5c4e485103ac57af7ffd7b60272b4a26a9ebd05368f9";

/// Logical node roles mapped to host-accessible execution RPC endpoints.
pub type EndpointMap = BTreeMap<String, String>;

/// Recovery metadata for resources created by this runner.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    /// Canonical repository root supplying Compose configuration.
    pub repo: PathBuf,
    /// Canonical run output directory.
    pub output: PathBuf,
    /// Unique Compose project name.
    pub project: String,
}

/// Exclusive Compose lifecycle preserving pre-existing developer resources.
#[derive(Debug)]
pub struct Provisioner {
    repo: PathBuf,
    output: PathBuf,
    project: String,
    owned: bool,
    private_owned: bool,
    lock_handle: Option<File>,
}

impl Provisioner {
    /// Creates an inert lifecycle owner; construction never touches Docker or a lock.
    pub fn new(repo: PathBuf, output: PathBuf, id: &str) -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        Self {
            repo,
            output,
            project: format!("base-acceptance-{id}-{nonce}"),
            owned: false,
            private_owned: false,
            lock_handle: None,
        }
    }

    /// Generates and verifies fresh chain state, then starts its consumers.
    pub async fn setup(&mut self, config: &ScenarioConfig, build: bool) -> Result<EndpointMap> {
        self.repo = self.repo.canonicalize()?;
        self.output = self.output.canonicalize()?;
        self.acquire_lock()?;
        let private = self.output.join("private");
        fs::create_dir(&private).wrap_err("acceptance requires a fresh private state directory")?;
        self.private_owned = true;
        Self::restrict(&private, true)?;
        fs::create_dir(private.join("devnet"))?;
        self.write_environment(config)?;
        self.write_profile(config)?;
        self.write_ports(config).await?;
        self.preflight().await?;
        if build {
            let mut command = Command::new("docker");
            command
                .args(["buildx", "bake", "--load", "-f", "etc/docker/docker-bake.hcl", "devnet"])
                .env("PROFILE", "dev")
                .env("BASE_SUCCINCT_ELF_REQUIRE", "0")
                .current_dir(&self.repo);
            Self::command(&mut command, Duration::from_secs(1800)).await?;
            self.compose(&["build", "setup-devnet"], Duration::from_secs(1800)).await?;
        }
        self.compose(&["pull", "--ignore-buildable"], Duration::from_secs(300)).await?;
        let model = self.model().await?;
        let mut images = BTreeSet::new();
        for service in model["services"]
            .as_object()
            .ok_or_else(|| eyre::eyre!("Compose services missing"))?
            .values()
        {
            if service.get("profiles").is_none()
                && let Some(image) = service["image"].as_str()
            {
                images.insert(image.to_owned());
            }
        }
        let mut provenance = Vec::new();
        for image in images {
            let inspected = Self::command(
                Command::new("docker").args([
                    "image",
                    "inspect",
                    "--format",
                    "{{json .Id}}",
                    &image,
                ]),
                Duration::from_secs(15),
            )
            .await?;
            provenance.push(json!({
                "image": image,
                "id": serde_json::from_str::<Value>(&inspected)?,
            }));
        }
        fs::create_dir_all(self.output.join("report"))?;
        fs::write(
            self.output.join("report/environment.json"),
            serde_json::to_vec_pretty(&json!({
                "images": provenance,
                "built_by_this_invocation": build,
                "project": self.project,
            }))?,
        )?;
        let owner = Ownership {
            repo: self.repo.clone(),
            output: self.output.clone(),
            project: self.project.clone(),
        };
        fs::write(private.join("ownership.json"), serde_json::to_vec_pretty(&owner)?)?;
        self.owned = true;
        // Refresh the pinned genesis timestamp after potentially lengthy image builds and pulls.
        self.write_environment(config)?;
        self.compose(
            &[
                "up",
                "--no-build",
                "--pull",
                "never",
                "--exit-code-from",
                "setup-devnet",
                "setup-devnet",
            ],
            config.readiness.timeout.0,
        )
        .await?;
        self.verify_artifacts(config)?;
        self.compose(
            &[
                "up",
                "-d",
                "--no-build",
                "--pull",
                "never",
                "--wait",
                "--wait-timeout",
                &config.readiness.timeout.0.as_secs().to_string(),
            ],
            config.readiness.timeout.0 + Duration::from_secs(15),
        )
        .await?;
        self.endpoints().await
    }

    /// Writes isolated ports and optional RPC-forwarding overrides.
    pub async fn write_ports(&self, config: &ScenarioConfig) -> Result<()> {
        let model = self.model().await?;
        fs::write(self.output.join("private/ports.yml"), Self::port_overlay(&model, config)?)?;
        Ok(())
    }

    /// Renders the isolated service overlay from Compose's resolved commands.
    pub fn port_overlay(model: &Value, config: &ScenarioConfig) -> Result<String> {
        let mut overlay = String::from("services:\n");
        for name in model["services"]
            .as_object()
            .ok_or_else(|| eyre::eyre!("Compose services missing"))?
            .keys()
        {
            let ports: &[u16] = match name.as_str() {
                "l1-el" => &[4545],
                "l1-cl" => &[4052],
                "base-builder" => &[7545, 7549, 7111, 7090],
                "base-client" => &[8545, 8549, 8090],
                "base-rpc" => &[8645],
                "base-shadow-validator" => &[8845],
                _ => &[],
            };
            overlay.push_str(&format!("  {}:\n    ports: !override", serde_json::to_string(name)?));
            if ports.is_empty() {
                overlay.push_str(" []\n");
            } else {
                for port in ports {
                    overlay.push_str(&format!("\n      - \"127.0.0.1::{port}\""));
                }
                overlay.push('\n');
            }
            if name == "base-rpc"
                && let Some(forwarding) = &config.devnet.l2.forwarding
            {
                let mut command: Vec<String> =
                    serde_json::from_value(model["services"][name]["command"].clone())?;
                command.push(format!("--tx-forwarding-max-rps={}", forwarding.max_rps));
                command.push(format!(
                    "--tx-forwarding-resend-after-ms={}",
                    forwarding.resend_after.0.as_millis()
                ));
                overlay.push_str(&format!(
                    "    command: !override {}\n",
                    serde_json::to_string(&command)?
                ));
            }
        }
        Ok(overlay)
    }

    /// Resolves the published host endpoint for each logical role after startup.
    pub async fn endpoints(&self) -> Result<EndpointMap> {
        let mut endpoints = BTreeMap::new();
        for (role, service, port) in [
            ("l1", "l1-el", "4545"),
            ("beacon", "l1-cl", "4052"),
            ("builder", "base-builder", "7545"),
            ("builder-consensus", "base-builder", "7549"),
            ("builder-flashblocks", "base-builder", "7111"),
            ("builder-metrics", "base-builder", "7090"),
            ("validator", "base-client", "8545"),
            ("validator-consensus", "base-client", "8549"),
            ("validator-metrics", "base-client", "8090"),
            ("rpc", "base-rpc", "8645"),
            ("shadow", "base-shadow-validator", "8845"),
        ] {
            if role == "shadow" && self.output.join("private/profile.yml").exists() {
                continue;
            }
            let address = self.compose(&["port", service, port], Duration::from_secs(10)).await?;
            endpoints.insert(role.into(), format!("http://{}", address.trim()));
        }
        Ok(endpoints)
    }

    /// Acquires a kernel lock; the stable lock file is never unlinked by any runner.
    pub fn acquire_lock(&mut self) -> Result<()> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        options.mode(0o600);
        let mut lock = options.open(std::env::temp_dir().join("base-acceptance-docker.lock"))?;
        lock.try_lock().wrap_err("another acceptance run owns the local Docker daemon")?;
        lock.set_len(0)?;
        writeln!(lock, "{}", std::process::id())?;
        self.lock_handle = Some(lock);
        Ok(())
    }

    /// Writes a private Compose environment with explicit scenario overrides.
    pub fn write_environment(&self, config: &ScenarioConfig) -> Result<()> {
        let root = self.output.join("private/devnet").canonicalize()?;
        if root.to_string_lossy().contains(['\n', '\r', '$', '#', '"', '\'']) {
            bail!("unsupported characters in output directory");
        }
        let mut env = fs::read_to_string(self.repo.join("etc/docker/devnet-env"))?;
        let genesis_timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 15;
        env.push_str(&format!(
            "\nDEVNET_ROOT={}\nPROFILE=dev\nBASE_SUCCINCT_ELF_REQUIRE=0\nL1_CHAIN_ID={}\nL2_CHAIN_ID={}\nL1_SLOT_DURATION_OVERRIDE={}\nBASE_DEVNET_TIMESTAMP={}\nBASE_DEVNET_VALIDATOR_COUNT={}\nBASE_NODE_VERIFIER_L1_CONFS={}\n",
            root.display(),
            config.devnet.l1.chain_id,
            config.devnet.l2.chain_id,
            config.devnet.l1.slot_duration.0.as_secs(),
            genesis_timestamp,
            config.devnet.l1.validator_count,
            config.devnet.l2.verifier_l1_confirmations,
        ));
        if let Some(fork) = &config.devnet.l1.forks.glamsterdam {
            let activation = fork
                .activation_epoch
                .checked_mul(8)
                .and_then(|slots| slots.checked_mul(config.devnet.l1.slot_duration.0.as_secs()))
                .and_then(|offset| genesis_timestamp.checked_add(offset))
                .ok_or_else(|| eyre::eyre!("Glamsterdam schedule overflow"))?;
            env.push_str(&format!(
                "BASE_DEVNET_AMSTERDAM_TIME={activation}\nBASE_DEVNET_GLOAS_EPOCH={}\n",
                fork.activation_epoch
            ));
        }
        for (name, activation) in &config.devnet.l2.forks {
            env.push_str(&format!(
                "L2_BASE_{}_BLOCK={}\n",
                name.to_ascii_uppercase(),
                activation.block().map_or_else(String::new, |block| block.to_string())
            ));
        }
        let path = self.output.join("private/compose.env");
        fs::write(&path, env)?;
        Self::restrict(&path, false)
    }

    /// Writes the reviewed profile-only Compose overrides.
    pub fn write_profile(&self, config: &ScenarioConfig) -> Result<()> {
        let path = self.output.join("private/profile.yml");
        if config.devnet.profile == DevnetProfile::Canonical {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        let overlay = format!(
            r#"services:
  l1-el:
    image: {RETH_GLAMSTERDAM_IMAGE}
  l1-cl:
    image: {LIGHTHOUSE_GLAMSTERDAM_IMAGE}
  l1-vc:
    image: {LIGHTHOUSE_GLAMSTERDAM_IMAGE}
  op-batcher:
    profiles: ["acceptance-disabled"]
  base-shadow-validator:
    profiles: ["acceptance-disabled"]
  base-batcher:
    command: !override
      - batcher
      - --l1-rpc-url=http://l1-el:${{L1_HTTP_PORT}}
      - --l2-rpc-url=http://base-builder:${{L2_BUILDER_HTTP_PORT}}
      - --private-key=${{BATCHER_KEY}}
      - --data-availability-type=blobs
      - --max-channel-duration=2
      - --poll-interval=1
      - --sub-safety-margin=0
      - --num-confirmations=1
      - --metrics.enabled
      - --metrics.addr=0.0.0.0
      - --metrics.port=${{SHADOW_BATCHER_METRICS_PORT}}
"#
        );
        fs::write(&path, overlay)?;
        Self::restrict(&path, false)
    }

    /// Rejects container name collisions without adopting existing resources.
    /// Docker assigns and binds the dynamically allocated host ports at startup.
    pub async fn preflight(&self) -> Result<()> {
        let model = self.model().await?;
        let existing = Self::command(
            Command::new("docker").args(["ps", "-a", "--format", "{{.Names}}"]),
            Duration::from_secs(15),
        )
        .await?;
        for service in model["services"]
            .as_object()
            .ok_or_else(|| eyre::eyre!("Compose services missing"))?
            .values()
        {
            if service.get("profiles").is_some() {
                continue;
            }
            if let Some(name) = service["container_name"].as_str()
                && existing.lines().any(|line| line == name)
            {
                bail!(
                    "container {name} already exists; stop the developer devnet explicitly before acceptance"
                );
            }
        }
        Ok(())
    }

    /// Resolves Compose configuration without persisting credential-bearing fields.
    pub async fn model(&self) -> Result<Value> {
        Ok(serde_json::from_str(
            &self.compose(&["config", "--format", "json"], Duration::from_secs(15)).await?,
        )?)
    }

    /// Verifies generated identities, schedules, validator material and activation pre-window.
    pub fn verify_artifacts(&self, config: &ScenarioConfig) -> Result<()> {
        let root = self.output.join("private/devnet");
        let rollup = root.join("l2/configs/rollup.json");
        let forks = config.verified_forks(&rollup)?;
        let genesis: Value =
            serde_json::from_slice(&fs::read(root.join("l2/configs/genesis.json"))?)?;
        if genesis.pointer("/config/chainId").and_then(Value::as_u64)
            != Some(config.devnet.l2.chain_id)
        {
            bail!("L2 genesis chain ID differs from scenario");
        }
        let l1: Value =
            serde_json::from_slice(&fs::read(root.join("l1/configs/el/genesis.json"))?)?;
        if l1.pointer("/config/chainId").and_then(Value::as_u64) != Some(config.devnet.l1.chain_id)
        {
            bail!("L1 genesis chain ID differs from scenario");
        }
        let chain: Value =
            serde_json::from_slice(&fs::read(root.join("l1/configs/el/chain-config.json"))?)?;
        if l1.get("config") != Some(&chain) {
            bail!("L1 genesis and Base node chain config differ")
        }
        let validators = fs::read_dir(root.join("l1/configs/cl/validator_data/validators"))?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .count() as u64;
        if validators != config.devnet.l1.validator_count {
            bail!("generated validator count {validators} differs from scenario")
        }
        if fs::metadata(root.join("l1/configs/cl/genesis.ssz"))?.len() == 0 {
            bail!("generated consensus genesis is empty")
        }
        if let Some(fork) = &config.devnet.l1.forks.glamsterdam {
            let activation = l1
                .pointer("/config/amsterdamTime")
                .and_then(Value::as_u64)
                .ok_or_else(|| eyre::eyre!("generated Amsterdam schedule is missing"))?;
            let genesis = l1
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|value| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok())
                .ok_or_else(|| eyre::eyre!("generated L1 timestamp is invalid"))?;
            let expected =
                genesis + fork.activation_epoch * 8 * config.devnet.l1.slot_duration.0.as_secs();
            let cl = fs::read_to_string(root.join("l1/configs/cl/config.yaml"))?;
            if activation != expected
                || !cl
                    .lines()
                    .any(|line| line == format!("GLOAS_FORK_EPOCH: {}", fork.activation_epoch))
            {
                bail!("generated Amsterdam and Gloas schedules do not match")
            }
            if SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() >= activation {
                bail!("Glamsterdam activation pre-window was missed")
            }
        }
        for (name, activation) in &config.devnet.l2.forks {
            let actual = genesis.pointer(&format!("/config/base/{name}")).and_then(Value::as_u64);
            let expected =
                forks.iter().find(|fork| &fork.name == name).map(|fork| fork.activation_timestamp);
            if actual != expected && !(activation.block() == Some(0) && actual == Some(0)) {
                bail!("L2 genesis {name} schedule differs from verified rollup");
            }
        }
        Ok(())
    }

    /// Captures bounded sanitized logs, returning collection failures truthfully.
    pub async fn collect_logs(&self) -> Result<()> {
        if !self.owned {
            return Ok(());
        }
        let logs = self
            .compose(
                &["logs", "--no-color", "--timestamps", "--tail", "1000"],
                Duration::from_secs(30),
            )
            .await?;
        let clean = self.sanitize(&logs);
        let evidence = self.output.join("report/evidence");
        fs::create_dir_all(&evidence)?;
        fs::write(evidence.join("compose.log"), clean)?;
        Ok(())
    }

    /// Redacts configured secrets and strips control characters from diagnostics.
    pub fn sanitize(&self, text: &str) -> String {
        let mut clean = text.replace(&self.output.to_string_lossy().to_string(), "<OUTPUT>");
        if let Ok(env) = fs::read_to_string(self.output.join("private/compose.env")) {
            for (key, value) in env.lines().filter_map(|line| line.split_once('=')) {
                if (key.contains("KEY")
                    || key.contains("TOKEN")
                    || key.contains("SECRET")
                    || key.contains("PASSWORD"))
                    && value.len() > 8
                {
                    clean = clean.replace(value.trim_matches(['\'', '"']), "[REDACTED]");
                }
            }
        }
        let mut characters = clean.chars().peekable();
        let mut output = String::new();
        while let Some(character) = characters.next() {
            if character == '\u{1b}' && characters.peek() == Some(&'[') {
                characters.next();
                for part in characters.by_ref() {
                    if ('@'..='~').contains(&part) {
                        break;
                    }
                }
            } else if !character.is_control() || matches!(character, '\n' | '\t') {
                output.push(character);
            }
            if output.len() >= 2_000_000 {
                break;
            }
        }
        output
    }

    /// Stops owned resources and deletes only this run's freshly generated state.
    /// Removing root-owned state requires the local `devnet-setup:local-v2` image.
    pub async fn cleanup(&mut self) -> Result<()> {
        if self.owned {
            self.compose(
                &["down", "--volumes", "--remove-orphans", "--timeout", "10"],
                Duration::from_secs(40),
            )
            .await?;
            let data = self.output.join("private/devnet");
            if data.exists() {
                Self::command(
                    Command::new("docker").args([
                        "run",
                        "--rm",
                        "--network",
                        "none",
                        "--entrypoint",
                        "/bin/sh",
                        "--mount",
                        &format!("type=bind,source={},target=/owned", data.display()),
                        "devnet-setup:local-v2",
                        "-c",
                        "find /owned -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +",
                    ]),
                    Duration::from_secs(30),
                )
                .await?;
            }
            self.owned = false;
        }
        if self.private_owned {
            fs::remove_dir_all(self.output.join("private"))?;
            self.private_owned = false;
        }
        self.lock_handle.take();
        Ok(())
    }

    /// Stops both batchers only in the invocation-owned Compose project.
    pub async fn stop_batchers(&self) -> Result<()> {
        if !self.owned || self.lock_handle.is_none() {
            bail!("cannot stop batchers without an owned, locked devnet");
        }
        self.compose(&["stop", "op-batcher", "base-batcher"], Duration::from_secs(30)).await?;
        Ok(())
    }

    /// Recovers an interrupted local run using its private ownership manifest.
    pub async fn recover(path: &Path) -> Result<()> {
        let canonical = path.canonicalize()?;
        let owner: Ownership = serde_json::from_slice(&fs::read(&canonical)?)?;
        if canonical != owner.output.join("private/ownership.json")
            || !owner.project.starts_with("base-acceptance-")
            || !owner
                .project
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            bail!("invalid ownership manifest");
        }
        let mut provisioner = Self {
            repo: owner.repo,
            output: owner.output,
            project: owner.project,
            owned: true,
            private_owned: true,
            lock_handle: None,
        };
        provisioner.acquire_lock()?;
        provisioner.cleanup().await
    }

    /// Invokes canonical Compose with scenario-sensitive ambient variables removed.
    pub async fn compose(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let env_file = self.output.join("private/compose.env");
        let mut command = Command::new("docker");
        command
            .args(["compose", "--project-name", &self.project, "--env-file"])
            .arg(&env_file)
            .arg("--file")
            .arg(self.repo.join("etc/docker/docker-compose.yml"));
        let ports = self.output.join("private/ports.yml");
        if ports.exists() {
            command.arg("--file").arg(ports);
        }
        let profile = self.output.join("private/profile.yml");
        if profile.exists() {
            command.arg("--file").arg(profile);
        }
        command.args(args).current_dir(&self.repo);
        for line in fs::read_to_string(&env_file)?.lines() {
            if let Some((key, _)) = line.split_once('=') {
                command.env_remove(key.trim());
            }
        }
        for (key, _) in std::env::vars().filter(|(key, _)| {
            key.starts_with("COMPOSE_")
                || key.starts_with("L2_BASE_")
                || key.starts_with("UPGRADE_SIGNAL_")
        }) {
            command.env_remove(key);
        }
        Self::command(&mut command, timeout)
            .await
            .map_err(|error| eyre::eyre!("{}", self.sanitize(&error.to_string())))
    }

    /// Executes a child with bounded retained output while draining both pipes.
    pub async fn command(command: &mut Command, timeout: Duration) -> Result<String> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| eyre::eyre!("stdout unavailable"))?;
        let stderr = child.stderr.take().ok_or_else(|| eyre::eyre!("stderr unavailable"))?;
        let (status, out, err) = tokio::time::timeout(timeout, async {
            let (out, err, status) =
                tokio::try_join!(Self::drain(stdout), Self::drain(stderr), child.wait())?;
            Ok::<_, std::io::Error>((status, out, err))
        })
        .await
        .wrap_err("command deadline elapsed")??;
        if !status.success() {
            bail!(
                "command failed ({status}): {}",
                String::from_utf8_lossy(&err).chars().take(4000).collect::<String>()
            );
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Retains a bounded prefix and drains the remainder to avoid deadlocking a child.
    pub async fn drain(mut input: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
        const LIMIT: usize = 2_000_000;
        let mut output = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let read = input.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..read.min(LIMIT.saturating_sub(output.len()))]);
        }
        Ok(output)
    }

    /// Restricts private state on Unix, where the Compose devnet is supported.
    pub fn restrict(path: &Path, directory: bool) -> Result<()> {
        #[cfg(unix)]
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
        Ok(())
    }

    /// Host endpoints of the canonical single-sequencer topology.
    pub fn default_endpoints() -> EndpointMap {
        [
            ("l1", 4545),
            ("beacon", 4052),
            ("builder", 7545),
            ("builder-consensus", 7549),
            ("builder-flashblocks", 7111),
            ("builder-metrics", 7090),
            ("validator", 8545),
            ("validator-consensus", 8549),
            ("validator-metrics", 8090),
            ("rpc", 8645),
            ("shadow", 8845),
        ]
        .into_iter()
        .map(|(role, port)| (role.into(), format!("http://127.0.0.1:{port}")))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(profile: DevnetProfile) -> ScenarioConfig {
        let source = match profile {
            DevnetProfile::Canonical => {
                r#"
schema_version = 1
id = "test"
description = "test"
[[checks]]
id = "identity"
kind = "chain_id"
endpoint = "builder"
expected = 84538453
timeout = "5s"
"#
            }
            DevnetProfile::Glamsterdam => {
                r#"
schema_version = 1
id = "test"
description = "test"
[devnet]
profile = "glamsterdam"
[devnet.l1]
validator_count = 64
slot_duration = "6s"
[devnet.l1.forks]
glamsterdam = {}
[[checks]]
id = "identity"
kind = "chain_id"
endpoint = "builder"
expected = 84538453
timeout = "5s"
"#
            }
        };
        toml::from_str(source).unwrap()
    }

    #[test]
    fn contender_cannot_release_owner_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let owner = File::create(&path).unwrap();
        owner.try_lock().unwrap();
        let contender = File::open(&path).unwrap();
        assert!(contender.try_lock().is_err());
        drop(contender);
        assert!(File::open(&path).unwrap().try_lock().is_err());
        drop(owner);
        File::open(&path).unwrap().try_lock().unwrap();
    }

    #[tokio::test]
    async fn output_is_bounded() {
        let data = vec![b'a'; 2_100_000];
        assert_eq!(Provisioner::drain(data.as_slice()).await.unwrap().len(), 2_000_000);
    }

    #[tokio::test]
    async fn inert_cleanup_preserves_existing_private_data() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("private")).unwrap();
        let sentinel = dir.path().join("private/sentinel");
        fs::write(&sentinel, "developer data").unwrap();
        let mut provisioner = Provisioner::new(PathBuf::new(), dir.path().into(), "test");
        provisioner.cleanup().await.unwrap();
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "developer data");
    }

    #[test]
    fn canonical_profile_uses_unmodified_compose() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("private")).unwrap();
        let provisioner = Provisioner::new(PathBuf::new(), dir.path().into(), "test");
        provisioner.write_profile(&scenario(DevnetProfile::Canonical)).unwrap();
        assert!(!dir.path().join("private/profile.yml").exists());
    }

    #[test]
    fn forwarding_overlay_preserves_rpc_command_and_does_not_modify_builder() {
        let command = json!(["--chain", "dev", "rpc", "--enable-tx-forwarding"]);
        let model = json!({"services": {
            "base-rpc": {"command": command},
            "base-builder": {"command": ["sequencer"]}
        }});
        let mut config = scenario(DevnetProfile::Canonical);
        let unchanged = Provisioner::port_overlay(&model, &config).unwrap();
        assert!(!unchanged.contains("command:"));
        config.devnet.l2.forwarding = Some(crate::ForwardingConfig {
            max_rps: 7,
            resend_after: crate::Span(Duration::from_millis(1750)),
        });
        let overlay = Provisioner::port_overlay(&model, &config).unwrap();
        let command_line =
            overlay.lines().find_map(|line| line.strip_prefix("    command: !override ")).unwrap();
        let actual: Vec<String> = serde_json::from_str(command_line).unwrap();
        assert_eq!(
            actual,
            [
                "--chain",
                "dev",
                "rpc",
                "--enable-tx-forwarding",
                "--tx-forwarding-max-rps=7",
                "--tx-forwarding-resend-after-ms=1750"
            ]
        );
        assert_eq!(overlay.matches("command:").count(), 1);
        assert!(overlay.contains(
            "\"base-rpc\":\n    ports: !override\n      - \"127.0.0.1::8645\"\n    command:"
        ));
    }

    #[tokio::test]
    #[ignore = "requires Docker Compose; renders configuration without starting containers"]
    async fn forwarding_overlay_renders_with_canonical_compose() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("private/devnet")).unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let provisioner = Provisioner::new(repo.into(), dir.path().into(), "compose-test");
        let config = ScenarioConfig::load(
            repo.join("acceptance/scenarios/system/transaction/high-load.toml"),
        )
        .unwrap();
        provisioner.write_environment(&config).unwrap();
        provisioner.write_profile(&config).unwrap();
        let original = provisioner.model().await.unwrap();
        provisioner.write_ports(&config).await.unwrap();
        let rendered = provisioner.model().await.unwrap();
        let command = rendered["services"]["base-rpc"]["command"].as_array().unwrap();
        let original_command = original["services"]["base-rpc"]["command"].as_array().unwrap();
        assert!(command.starts_with(original_command));
        assert_eq!(
            &command[original_command.len()..],
            &[json!("--tx-forwarding-max-rps=1"), json!("--tx-forwarding-resend-after-ms=30000")]
        );
        assert_eq!(
            rendered["services"]["base-builder"]["command"],
            original["services"]["base-builder"]["command"]
        );
    }

    #[tokio::test]
    async fn stopping_batchers_requires_owned_resources() {
        let dir = tempfile::tempdir().unwrap();
        let provisioner = Provisioner::new(PathBuf::new(), dir.path().into(), "test");
        assert_eq!(
            provisioner.stop_batchers().await.unwrap_err().to_string(),
            "cannot stop batchers without an owned, locked devnet"
        );
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn glamsterdam_profile_owns_clients_and_canonical_batcher() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("private")).unwrap();
        let provisioner = Provisioner::new(PathBuf::new(), dir.path().into(), "test");
        provisioner.write_profile(&scenario(DevnetProfile::Glamsterdam)).unwrap();
        let overlay = fs::read_to_string(dir.path().join("private/profile.yml")).unwrap();
        assert!(overlay.contains(RETH_GLAMSTERDAM_IMAGE));
        assert!(overlay.contains(LIGHTHOUSE_GLAMSTERDAM_IMAGE));
        assert!(overlay.contains("--private-key=${BATCHER_KEY}"));
        assert!(overlay.contains("--data-availability-type=blobs"));
        assert_eq!(overlay.matches("profiles: [\"acceptance-disabled\"]").count(), 2);
        assert!(!overlay.contains("SHADOW_BATCHER_KEY"));
        assert!(!overlay.contains("--shadow-mode"));
    }

    #[test]
    fn diagnostic_removes_ansi_and_configured_credentials() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("private")).unwrap();
        fs::write(dir.path().join("private/compose.env"), "PRIVATE_KEY=never-publish-this\n")
            .unwrap();
        let provisioner = Provisioner::new(PathBuf::new(), dir.path().into(), "test");
        assert_eq!(
            provisioner.sanitize("\x1b[31mERROR\x1b[0m never-publish-this\n"),
            "ERROR [REDACTED]\n"
        );
    }
}
