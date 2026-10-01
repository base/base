use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    time::Duration,
};

use eyre::{Result, bail};
use serde_json::json;
use tokio::time::Instant;

use crate::{
    AcceptanceCheck, Aggregate, CheckResult, CheckStart, EndpointMap, ObservationState,
    Provisioner, RpcObserver, ScenarioConfig, ScenarioResult, StageResult, Status,
};

/// Invocation-owned paths, attach endpoints, and image build policy.
#[derive(Debug, Clone)]
pub struct AcceptanceOptions {
    /// Repository containing the managed devnet assets.
    pub repo_root: PathBuf,
    /// Report and managed-devnet output directory.
    pub output: PathBuf,
    /// Logical RPC endpoints for read-only attach mode.
    pub endpoints: Option<BTreeMap<String, String>>,
    /// Verified rollup configuration used in attach mode.
    pub rollup: Option<PathBuf>,
    /// Whether managed service images should be rebuilt.
    pub build: bool,
}

/// Runs bounded acceptance scenarios, including cleanup after failures and signals.
#[derive(Debug)]
pub struct AcceptanceRunner;

impl AcceptanceRunner {
    /// Runs a scenario, preserving every expected check and lifecycle failure.
    pub async fn run(config: ScenarioConfig, options: AcceptanceOptions) -> Result<ScenarioResult> {
        config.validate()?;
        if options.endpoints.is_some() && options.build {
            bail!("attach mode cannot build images");
        }
        if let Some(endpoints) = &options.endpoints {
            Self::validate_endpoints(&config, endpoints)?;
        }
        let started = Instant::now();
        let mut result = ScenarioResult {
            id: config.id.clone(),
            status: Status::Error,
            duration_ms: 0,
            config: serde_json::to_value(&config)?,
            stages: Vec::new(),
            checks: config
                .checks
                .iter()
                .map(|check| Self::blocked(check, "prerequisites not completed"))
                .collect(),
            samples: Vec::new(),
            forks: Vec::new(),
            diagnostics: Vec::new(),
            reproduction: String::new(),
        };
        result.config["mode"] =
            json!(if options.endpoints.is_some() { "attach" } else { "managed" });
        let mut provisioner =
            Provisioner::new(options.repo_root.clone(), options.output.clone(), &config.id);
        // Cold image compilation has a separate bounded allowance; observation windows do not.
        let deadline = started
            + config.timeout.0
            + if options.build { Duration::from_secs(3600) } else { Duration::ZERO };
        let operation = tokio::select! {
            execution = tokio::time::timeout_at(
                deadline,
                Self::execute(
                    &config,
                    &options,
                    &mut provisioner,
                    &mut result,
                    started,
                    deadline,
                ),
            ) => {
                execution
                    .map_err(|_| eyre::eyre!("scenario deadline elapsed"))
                    .and_then(std::convert::identity)
            }
            () = Self::interrupted() => {
                for check in &mut result.checks {
                    if check.status == Status::Blocked {
                        check.status = Status::Cancelled;
                        check.message = "execution interrupted".into();
                    }
                }
                result.stages.push(Self::stage(
                    "interruption",
                    Status::Cancelled,
                    started,
                    "received shutdown signal",
                ));
                Ok(())
            }
        };
        if let Err(error) = operation {
            let message = provisioner.sanitize(&error.to_string());
            let stage = if result.stages.iter().any(|stage| stage.id == "setup") {
                "execution"
            } else {
                "setup"
            };
            result.stages.push(Self::stage(stage, Status::Error, started, &message));
            result.diagnostics.push(message);
        }
        let check_status = [Status::Error, Status::Cancelled, Status::Failed, Status::Blocked]
            .into_iter()
            .find(|status| result.checks.iter().any(|check| check.status == *status))
            .unwrap_or(Status::Passed);
        result.stages.push(StageResult {
            id: "checks".into(),
            status: check_status,
            duration_ms: result.checks.iter().map(|check| check.duration_ms).sum(),
            message: "ordered acceptance observations; see individual outcomes below".into(),
        });
        if options.endpoints.is_none() {
            let collect = Instant::now();
            let logs = provisioner.collect_logs().await;
            result.stages.push(Self::stage(
                "collect",
                if logs.is_ok() { Status::Passed } else { Status::Error },
                collect,
                &logs.err().map_or_else(
                    || "evidence collection completed for owned resources".into(),
                    |error| error.to_string(),
                ),
            ));
            let cleanup = Instant::now();
            let cleanup_result =
                tokio::time::timeout(Duration::from_secs(80), provisioner.cleanup()).await;
            let message = match cleanup_result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => Some(
                    "cleanup deadline elapsed; use the private ownership manifest to recover"
                        .into(),
                ),
            };
            result.stages.push(Self::stage(
                "cleanup",
                if message.is_none() { Status::Passed } else { Status::Error },
                cleanup,
                message.as_deref().unwrap_or("owned resources removed"),
            ));
        }
        let evidence = options.output.join("report/evidence");
        fs::create_dir_all(&evidence)?;
        fs::write(evidence.join("heads.json"), serde_json::to_vec_pretty(&result.samples)?)?;
        let environment = options.output.join("report/environment.json");
        if environment.exists() {
            result.config["environment"] =
                serde_json::from_slice(&Aggregate::bounded_read(&environment)?)?;
        }
        for check in &mut result.checks {
            check.evidence.push("evidence/heads.json".into());
            if evidence.join("compose.log").exists() {
                check.evidence.push("evidence/compose.log".into());
            }
        }
        result.duration_ms = started.elapsed().as_millis() as u64;
        result.status = result.outcome();
        Ok(result)
    }

    /// Performs the cancellable portion; the caller owns final evidence and teardown.
    pub async fn execute(
        config: &ScenarioConfig,
        options: &AcceptanceOptions,
        provisioner: &mut Provisioner,
        result: &mut ScenarioResult,
        origin: Instant,
        deadline: Instant,
    ) -> Result<()> {
        let setup = Instant::now();
        let endpoints = match &options.endpoints {
            Some(endpoints) => endpoints.clone(),
            None => provisioner.setup(config, options.build).await?,
        };
        let deadline = deadline.min(Instant::now() + config.timeout.0);
        result.stages.push(Self::stage(
            "setup",
            Status::Passed,
            setup,
            if options.endpoints.is_some() {
                "attached read-only; no resources created"
            } else {
                "fresh deployment and genesis verified"
            },
        ));
        Self::validate_endpoints(config, &endpoints)?;
        let needs_forks = config.checks.iter().any(|check| check.start().is_some());
        if options.endpoints.is_none() || needs_forks || options.rollup.is_some() {
            let path = match &options.rollup {
                Some(path) => path.clone(),
                None if options.endpoints.is_none() => {
                    options.output.join("private/devnet/l2/configs/rollup.json")
                }
                None => bail!("fork-window checks in attach mode require --rollup"),
            };
            result.forks = config.verified_forks(&path)?;
        }
        let observer =
            RpcObserver::new(config.readiness.request_timeout.0, config.readiness.poll_interval.0)?;
        let ready = Instant::now();
        let readiness_deadline = (ready + config.readiness.timeout.0).min(deadline);
        let readiness = tokio::time::timeout_at(
            readiness_deadline,
            Self::wait_ready(&observer, config, &endpoints, readiness_deadline),
        )
        .await;
        let readiness_error = match readiness {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some("readiness deadline elapsed without identity and progress".into()),
        };
        result.stages.push(Self::stage(
            "readiness",
            if readiness_error.is_none() { Status::Passed } else { Status::Error },
            ready,
            readiness_error
                .as_deref()
                .unwrap_or("required endpoints have the expected identity and advance"),
        ));
        if readiness_error.is_some() {
            return Ok(());
        }
        for (index, check) in config.checks.iter().enumerate() {
            if Instant::now() >= deadline {
                break;
            }
            if let Some(window) = check.start()
                && let Some((status, message)) = Self::open_window(
                    &observer, config, &endpoints, window, result, origin, deadline,
                )
                .await?
            {
                result.checks[index].status = status;
                result.checks[index].message = message;
                continue;
            }
            result.checks[index] =
                observer.run(check, &endpoints, &mut result.samples, origin, deadline).await;
            if result.checks[index].status == Status::Passed
                && let Some(name) = check.start().and_then(|window| window.before_fork.as_ref())
                && let Some(message) = Self::confirm_pre_fork(
                    &observer, config, &endpoints, name, result, origin, deadline,
                )
                .await?
            {
                result.checks[index].status = Status::Error;
                result.checks[index].message = message;
            }
        }
        Ok(())
    }

    /// Waits until a check's fork window opens.
    ///
    /// Returns `None` when the check may run, otherwise the status and message to record.
    /// Builder RPC failures are retried at the poll interval until `deadline`. An unreached
    /// `after_fork` activation is `Failed` when the builder answered after its last failure
    /// and `Error` when it was unavailable; a missed `before_fork` window is `Error`.
    pub async fn open_window(
        observer: &RpcObserver,
        config: &ScenarioConfig,
        endpoints: &EndpointMap,
        window: &CheckStart,
        result: &mut ScenarioResult,
        origin: Instant,
        deadline: Instant,
    ) -> Result<Option<(Status, String)>> {
        let name = window
            .before_fork
            .as_ref()
            .or(window.after_fork.as_ref())
            .ok_or_else(|| eyre::eyre!("missing fork window"))?;
        let fork = result
            .forks
            .iter()
            .position(|fork| &fork.name == name)
            .ok_or_else(|| eyre::eyre!("fork {name} not present in verified schedule"))?;
        let boundary = result.forks[fork].activation_timestamp;
        let builder = RpcObserver::endpoint(endpoints, "builder")?;
        let mut last_head = None;
        let mut last_error = None;
        loop {
            match observer.block(builder, "latest", deadline).await {
                Ok(block) => {
                    ObservationState::new(json!({}), &mut result.samples, origin)
                        .sample("builder", Some(&block));
                    let active = block.timestamp >= boundary;
                    if active && result.forks[fork].observed_block.is_none() {
                        result.forks[fork].observed_block = Some(block.number);
                        result.forks[fork].observed_elapsed_ms =
                            Some(origin.elapsed().as_millis() as u64);
                    }
                    if window.before_fork.is_some() {
                        return Ok(active.then(|| {
                            (Status::Error, format!("missed pre-activation window for {name}"))
                        }));
                    }
                    if active {
                        return Ok(None);
                    }
                    last_head = Some(block.number);
                }
                Err(error) => {
                    ObservationState::new(json!({}), &mut result.samples, origin)
                        .sample("builder", None);
                    // A request cut short by the deadline does not erase the last observed head.
                    if Instant::now() < deadline {
                        last_head = None;
                    }
                    last_error = Some(error);
                }
            }
            if Instant::now() >= deadline {
                return Ok(Some(match (last_head, last_error) {
                    (Some(head), _) => (
                        Status::Failed,
                        format!(
                            "builder head {head} did not reach {name} activation before the deadline"
                        ),
                    ),
                    (None, error) => (
                        Status::Error,
                        format!(
                            "builder unavailable while waiting for {name}: {}",
                            error.map_or_else(|| "no response".into(), |error| error.to_string())
                        ),
                    ),
                }));
            }
            tokio::time::sleep_until(
                (Instant::now() + config.readiness.poll_interval.0).min(deadline),
            )
            .await;
        }
    }

    /// Confirms that a passed `before_fork` check finished before its activation.
    ///
    /// Returns the error message to record when coverage crossed the activation or could not
    /// be confirmed before `deadline`. Builder RPC failures are retried at the poll interval.
    pub async fn confirm_pre_fork(
        observer: &RpcObserver,
        config: &ScenarioConfig,
        endpoints: &EndpointMap,
        name: &str,
        result: &mut ScenarioResult,
        origin: Instant,
        deadline: Instant,
    ) -> Result<Option<String>> {
        let boundary = result
            .forks
            .iter()
            .find(|fork| fork.name == name)
            .ok_or_else(|| eyre::eyre!("fork {name} not present in verified schedule"))?
            .activation_timestamp;
        let builder = RpcObserver::endpoint(endpoints, "builder")?;
        loop {
            match observer.block(builder, "latest", deadline).await {
                Ok(block) => {
                    ObservationState::new(json!({}), &mut result.samples, origin)
                        .sample("builder", Some(&block));
                    return Ok((block.timestamp >= boundary).then(|| {
                        format!("check window crossed {name} activation; pre-fork coverage is incomplete")
                    }));
                }
                Err(error) => {
                    ObservationState::new(json!({}), &mut result.samples, origin)
                        .sample("builder", None);
                    if Instant::now() >= deadline {
                        return Ok(Some(format!(
                            "could not confirm pre-fork coverage for {name}: {error}"
                        )));
                    }
                }
            }
            tokio::time::sleep_until(
                (Instant::now() + config.readiness.poll_interval.0).min(deadline),
            )
            .await;
        }
    }

    /// Waits for correct chain identity and a new head on every referenced endpoint.
    pub async fn wait_ready(
        observer: &RpcObserver,
        config: &ScenarioConfig,
        endpoints: &EndpointMap,
        deadline: Instant,
    ) -> Result<()> {
        let roles = Self::required_roles(config);
        let mut initial = BTreeMap::new();
        loop {
            let mut all = true;
            for role in &roles {
                let url = RpcObserver::endpoint(endpoints, role)?;
                match observer.chain_id(url, deadline).await {
                    Ok(chain) => {
                        let expected = if role == "l1" {
                            config.devnet.l1.chain_id
                        } else {
                            config.devnet.l2.chain_id
                        };
                        if chain != expected {
                            bail!(
                                "{role} chain identity mismatch: expected {expected}, observed {chain}"
                            );
                        }
                    }
                    Err(_) => {
                        all = false;
                        continue;
                    }
                }
                match observer.block(url, "latest", deadline).await {
                    Ok(block) => {
                        let first = initial.entry(role).or_insert(block.number);
                        all &= block.number > *first;
                    }
                    Err(_) => all = false,
                }
            }
            if all {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("readiness deadline elapsed");
            }
            tokio::time::sleep_until(
                (Instant::now() + config.readiness.poll_interval.0).min(deadline),
            )
            .await;
        }
    }

    /// Resolves dependencies from checks, including their fork observation role.
    pub fn required_roles(config: &ScenarioConfig) -> BTreeSet<String> {
        let mut roles = BTreeSet::new();
        for check in &config.checks {
            match check {
                AcceptanceCheck::HeadsConverge { endpoints, .. } => {
                    roles.extend(endpoints.iter().cloned())
                }
                AcceptanceCheck::ChainId { endpoint, .. }
                | AcceptanceCheck::HeadProgress { endpoint, .. }
                | AcceptanceCheck::SafeHeadProgress { endpoint, .. }
                | AcceptanceCheck::HeadFresh { endpoint, .. } => {
                    roles.insert(endpoint.clone());
                }
            }
            if check.start().is_some() {
                roles.insert("builder".into());
            }
        }
        roles
    }

    /// Rejects missing roles and credential-bearing URLs before any network operation.
    pub fn validate_endpoints(config: &ScenarioConfig, endpoints: &EndpointMap) -> Result<()> {
        for role in Self::required_roles(config) {
            if !endpoints.contains_key(&role) {
                bail!("required endpoint {role} missing");
            }
        }
        for (role, url) in endpoints {
            if !matches!(role.as_str(), "l1" | "builder" | "validator" | "rpc" | "shadow") {
                bail!("unknown endpoint role");
            }
            let parsed =
                reqwest::Url::parse(url).map_err(|_| eyre::eyre!("invalid URL for {role}"))?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                bail!("{role} requires an HTTP URL without credentials, query, or fragment");
            }
        }
        Ok(())
    }

    /// Builds a lifecycle stage without interpreting an error as an assertion failure.
    pub fn stage(id: &str, status: Status, started: Instant, message: &str) -> StageResult {
        StageResult {
            id: id.into(),
            status,
            duration_ms: started.elapsed().as_millis() as u64,
            message: message.into(),
        }
    }

    /// Preserves an expected check when a prerequisite prevents evaluation.
    pub fn blocked(check: &AcceptanceCheck, message: &str) -> CheckResult {
        CheckResult {
            id: check.id().into(),
            kind: check.kind().into(),
            status: Status::Blocked,
            duration_ms: 0,
            expected: json!({ "configuration": check }),
            observed: json!({}),
            message: message.into(),
            next_step: "inspect lifecycle errors and rerun with a fresh output directory".into(),
            samples: 0,
            rpc_errors: 0,
            evidence: Vec::new(),
        }
    }

    /// Waits for Ctrl-C or termination; cleanup remains outside the cancelled future.
    pub async fn interrupted() {
        #[cfg(unix)]
        {
            if let Ok(mut terminate) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::{CliRun, ForkBoundary, Span};

    const DENIM_ACTIVATION: u64 = 100;

    /// Serves scripted HTTP replies in order, repeating the last reply indefinitely.
    async fn scripted(replies: Vec<(u16, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut replies = VecDeque::from(replies);
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                let (status, body) = if replies.len() > 1 {
                    replies.pop_front().unwrap()
                } else {
                    replies[0].clone()
                };
                let mut request = [0; 8192];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 {status} Scripted\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{address}")
    }

    fn head(number: u64, timestamp: u64) -> (u16, String) {
        let result = json!({
            "number": format!("0x{number:x}"),
            "timestamp": format!("0x{timestamp:x}"),
            "hash": format!("0x{}", "a".repeat(64)),
        });
        (200, json!({ "jsonrpc": "2.0", "id": 1, "result": result }).to_string())
    }

    fn unavailable() -> (u16, String) {
        (503, String::new())
    }

    /// Runs one fork-window wait against a scripted builder with a short poll interval.
    async fn window(
        replies: Vec<(u16, String)>,
        before: bool,
        budget: Duration,
    ) -> (Option<(Status, String)>, ScenarioResult) {
        let mut config = ScenarioConfig::load("scenarios/smoke.toml").unwrap();
        config.readiness.poll_interval = Span(Duration::from_millis(10));
        let endpoints = BTreeMap::from([("builder".into(), scripted(replies).await)]);
        let mut result = CliRun::fatal(&config, String::new());
        result.forks.push(ForkBoundary {
            name: "denim".into(),
            chain: "l2".into(),
            activation_timestamp: DENIM_ACTIVATION,
            observed_block: None,
            observed_elapsed_ms: None,
        });
        let start = CheckStart {
            chain: "l2".into(),
            before_fork: before.then(|| "denim".into()),
            after_fork: (!before).then(|| "denim".into()),
        };
        let observer = RpcObserver::new(Duration::from_secs(1), Duration::from_millis(10)).unwrap();
        let outcome = AcceptanceRunner::open_window(
            &observer,
            &config,
            &endpoints,
            &start,
            &mut result,
            Instant::now(),
            Instant::now() + budget,
        )
        .await
        .unwrap();
        (outcome, result)
    }

    #[tokio::test]
    async fn fork_wait_retries_transient_builder_failures() {
        let (outcome, result) = window(
            vec![head(10, DENIM_ACTIVATION - 1), unavailable(), head(11, DENIM_ACTIVATION)],
            false,
            Duration::from_secs(5),
        )
        .await;
        assert!(outcome.is_none());
        assert_eq!(result.forks[0].observed_block, Some(11));
        assert!(result.samples.iter().any(|sample| sample.number.is_none()));
    }

    #[tokio::test]
    async fn unreached_fork_fails_when_builder_stalls_and_errors_when_unavailable() {
        let (outcome, _) =
            window(vec![head(10, DENIM_ACTIVATION - 1)], false, Duration::from_millis(100)).await;
        let (status, message) = outcome.unwrap();
        assert_eq!(status, Status::Failed);
        assert!(message.contains("builder head 10 did not reach denim"));

        let (outcome, _) = window(vec![unavailable()], false, Duration::from_millis(100)).await;
        let (status, message) = outcome.unwrap();
        assert_eq!(status, Status::Error);
        assert!(message.contains("builder unavailable while waiting for denim"));
    }

    #[tokio::test]
    async fn before_fork_window_retries_and_reports_missed_activation() {
        let (outcome, _) = window(
            vec![unavailable(), head(10, DENIM_ACTIVATION - 1)],
            true,
            Duration::from_secs(5),
        )
        .await;
        assert!(outcome.is_none());

        let (outcome, _) =
            window(vec![head(30, DENIM_ACTIVATION + 1)], true, Duration::from_secs(5)).await;
        assert_eq!(
            outcome.unwrap(),
            (Status::Error, "missed pre-activation window for denim".to_owned())
        );
    }

    #[tokio::test]
    async fn wrong_chain_blocks_every_check_without_owning_attached_resources() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let body = r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let config = ScenarioConfig::load("scenarios/smoke.toml").unwrap();
        let result = AcceptanceRunner::run(
            config,
            AcceptanceOptions {
                repo_root: PathBuf::new(),
                output: dir.path().into(),
                endpoints: Some(BTreeMap::from([
                    ("builder".into(), format!("http://{address}")),
                    ("validator".into(), format!("http://{address}")),
                ])),
                rollup: None,
                build: false,
            },
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(result.outcome(), Status::Error);
        assert_eq!(result.checks.len(), 3);
        assert!(result.checks.iter().all(|check| check.status == Status::Blocked));
        assert!(
            result.stages.iter().any(|stage| stage.id == "readiness"
                && stage.message.contains("chain identity mismatch"))
        );
        assert!(
            !result.stages.iter().any(|stage| matches!(stage.id.as_str(), "cleanup" | "collect"))
        );
        assert!(!dir.path().join("private").exists());
        assert!(dir.path().join("report/evidence/heads.json").is_file());
    }

    #[tokio::test]
    async fn fork_windows_require_builder_even_when_checks_use_another_role() {
        for window in ["before_fork", "after_fork"] {
            let config: ScenarioConfig = toml::from_str(&format!(
                r#"
schema_version = 1
id = "fork-window"
description = "Observe a validator across a builder fork boundary"
[[checks]]
id = "identity"
kind = "chain_id"
endpoint = "validator"
expected = 84538453
timeout = "5s"
start = {{ {window} = "denim", chain = "l2" }}
"#
            ))
            .unwrap();
            config.validate().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let error = AcceptanceRunner::run(
                config,
                AcceptanceOptions {
                    repo_root: PathBuf::new(),
                    output: dir.path().into(),
                    endpoints: Some(BTreeMap::from([(
                        "validator".into(),
                        "http://127.0.0.1:1".into(),
                    )])),
                    rollup: None,
                    build: false,
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), "required endpoint builder missing");
            assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
        }
    }

    #[tokio::test]
    async fn readiness_rejects_missing_endpoints_when_called_directly() {
        let config = ScenarioConfig::load("scenarios/smoke.toml").unwrap();
        let observer =
            RpcObserver::new(config.readiness.request_timeout.0, config.readiness.poll_interval.0)
                .unwrap();
        let error = AcceptanceRunner::wait_ready(
            &observer,
            &config,
            &BTreeMap::new(),
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "endpoint builder was not resolved");
    }

    #[tokio::test]
    async fn oversized_environment_is_rejected_before_json_parsing() {
        let config = ScenarioConfig::load("scenarios/denim-transition.toml").unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("report")).unwrap();
        fs::File::create(dir.path().join("report/environment.json"))
            .unwrap()
            .set_len(20 * 1024 * 1024 + 1)
            .unwrap();
        // Missing rollup stops execution before RPC calls; final evidence still gets validated.
        let error = AcceptanceRunner::run(
            config,
            AcceptanceOptions {
                repo_root: PathBuf::new(),
                output: dir.path().into(),
                endpoints: Some(Provisioner::default_endpoints()),
                rollup: None,
                build: false,
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("input exceeds 20 MiB"));
    }

    #[test]
    fn missing_or_credential_bearing_endpoints_are_rejected() {
        let config = ScenarioConfig::load("scenarios/smoke.toml").unwrap();
        assert!(AcceptanceRunner::validate_endpoints(&config, &BTreeMap::new()).is_err());
        let mut endpoints = Provisioner::default_endpoints();
        endpoints.insert("builder".into(), "http://secret:password@localhost:7545".into());
        assert!(AcceptanceRunner::validate_endpoints(&config, &endpoints).is_err());
    }
}
