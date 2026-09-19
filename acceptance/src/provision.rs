#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    net::{TcpListener, UdpSocket},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, process::Command};

use crate::ScenarioConfig;

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
        self.write_ports().await?;
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
            provenance.push(json!({"image":image,"id":serde_json::from_str::<Value>(&inspected)?}));
        }
        fs::create_dir_all(self.output.join("report"))?;
        fs::write(
            self.output.join("report/environment.json"),
            serde_json::to_vec_pretty(
                &json!({"images":provenance,"built_by_this_invocation":build,"project":self.project}),
            )?,
        )?;
        let owner = Ownership {
            repo: self.repo.clone(),
            output: self.output.clone(),
            project: self.project.clone(),
        };
        fs::write(private.join("ownership.json"), serde_json::to_vec_pretty(&owner)?)?;
        self.owned = true;
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

    /// Exposes only execution RPCs on daemon-allocated localhost ports.
    pub async fn write_ports(&self) -> Result<()> {
        let model = self.model().await?;
        let mut overlay = String::from("services:\n");
        for name in model["services"]
            .as_object()
            .ok_or_else(|| eyre::eyre!("Compose services missing"))?
            .keys()
        {
            let port = match name.as_str() {
                "l1-el" => Some(4545),
                "base-builder" => Some(7545),
                "base-client" => Some(8545),
                "base-rpc" => Some(8645),
                "base-shadow-validator" => Some(8845),
                _ => None,
            };
            overlay.push_str(&format!("  {name}:\n    ports: !override"));
            if let Some(port) = port {
                overlay.push_str(&format!("\n      - \"127.0.0.1::{port}\"\n"));
            } else {
                overlay.push_str(" []\n");
            }
        }
        fs::write(self.output.join("private/ports.yml"), overlay)?;
        Ok(())
    }

    /// Resolves the published host endpoint for each logical role after startup.
    pub async fn endpoints(&self) -> Result<EndpointMap> {
        let mut endpoints = BTreeMap::new();
        for (role, service, port) in [
            ("l1", "l1-el", "4545"),
            ("builder", "base-builder", "7545"),
            ("validator", "base-client", "8545"),
            ("rpc", "base-rpc", "8645"),
            ("shadow", "base-shadow-validator", "8845"),
        ] {
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
            bail!("unsupported characters in output directory")
        }
        let mut env = fs::read_to_string(self.repo.join("etc/docker/devnet-env"))?;
        env.push_str(&format!("\nDEVNET_ROOT={}\nPROFILE=dev\nBASE_SUCCINCT_ELF_REQUIRE=0\nL1_CHAIN_ID={}\nL2_CHAIN_ID={}\nL1_SLOT_DURATION_OVERRIDE={}\nBASE_NODE_VERIFIER_L1_CONFS={}\n", root.display(), config.devnet.l1.chain_id, config.devnet.l2.chain_id, config.devnet.l1.slot_duration.0.as_secs(), config.devnet.l2.verifier_l1_confirmations));
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

    /// Rejects name and host-port collisions without adopting existing resources.
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
                )
            }
            for port in service["ports"].as_array().into_iter().flatten() {
                if let Some(port_number) = port["published"].as_str() {
                    let address = format!("0.0.0.0:{port_number}");
                    if port["protocol"] == "udp" {
                        UdpSocket::bind(&address)
                            .wrap_err_with(|| format!("UDP port {port_number} is in use"))?;
                    } else {
                        TcpListener::bind(&address)
                            .wrap_err_with(|| format!("TCP port {port_number} is in use"))?;
                    }
                }
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

    /// Verifies generated identity and both representations of every L2 schedule.
    pub fn verify_artifacts(&self, config: &ScenarioConfig) -> Result<()> {
        let root = self.output.join("private/devnet");
        let rollup = root.join("l2/configs/rollup.json");
        let forks = config.verified_forks(&rollup)?;
        let genesis: Value =
            serde_json::from_slice(&fs::read(root.join("l2/configs/genesis.json"))?)?;
        if genesis.pointer("/config/chainId").and_then(Value::as_u64)
            != Some(config.devnet.l2.chain_id)
        {
            bail!("L2 genesis chain ID differs from scenario")
        }
        let l1: Value =
            serde_json::from_slice(&fs::read(root.join("l1/configs/el/genesis.json"))?)?;
        if l1.pointer("/config/chainId").and_then(Value::as_u64) != Some(config.devnet.l1.chain_id)
        {
            bail!("L1 genesis chain ID differs from scenario")
        }
        for (name, activation) in &config.devnet.l2.forks {
            let actual = genesis.pointer(&format!("/config/base/{name}")).and_then(Value::as_u64);
            let expected =
                forks.iter().find(|fork| &fork.name == name).map(|fork| fork.activation_timestamp);
            if actual != expected && !(activation.block() == Some(0) && actual == Some(0)) {
                bail!("L2 genesis {name} schedule differs from verified rollup")
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
            bail!("invalid ownership manifest")
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
            )
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
        [("l1", 4545), ("builder", 7545), ("validator", 8545), ("rpc", 8645), ("shadow", 8845)]
            .into_iter()
            .map(|(role, port)| (role.into(), format!("http://127.0.0.1:{port}")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
