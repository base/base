use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use eyre::{Result, bail, eyre};
use reqwest::{Client, redirect::Policy};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::{AcceptanceCheck, CheckResult, HeadSample, Status};

const RESPONSE_LIMIT: usize = 1_048_576;
const SAMPLE_LIMIT: usize = 10_000;

/// A validated block returned by an Ethereum JSON-RPC endpoint.
#[derive(Debug, Clone)]
pub struct ObservedBlock {
    /// Block height reported by the endpoint.
    pub number: u64,
    /// Unix timestamp in the block header.
    pub timestamp: u64,
    /// Canonical 32-byte block hash.
    pub hash: String,
}

/// Mutable evidence and counters shared by one acceptance check.
///
/// Samples use the scenario-wide `origin`; once the report limit is reached,
/// `samples_truncated` records that further observations were not retained.
#[derive(Debug)]
pub struct ObservationState<'a> {
    /// Number of valid RPC observations.
    pub count: u64,
    /// Number of failed RPC observations, including recovered failures.
    pub errors: u64,
    /// Whether infrastructure prevented complete evaluation.
    pub infrastructure_error: bool,
    /// Machine-readable expected value.
    pub expected: Value,
    /// Machine-readable latest observation and failure thresholds.
    pub observed: Value,
    /// Scenario-wide head sample collection.
    pub samples: &'a mut Vec<HeadSample>,
    /// Scenario-wide monotonic sample origin.
    pub origin: Instant,
    /// Whether the sample collection reached its fixed report limit.
    pub samples_truncated: bool,
}

impl<'a> ObservationState<'a> {
    /// Creates state for a check and its scenario-wide sample stream.
    pub fn new(expected: Value, samples: &'a mut Vec<HeadSample>, origin: Instant) -> Self {
        Self {
            count: 0,
            errors: 0,
            infrastructure_error: false,
            expected,
            observed: json!({}),
            samples,
            origin,
            samples_truncated: false,
        }
    }

    /// Records a valid block or an explicit sampling gap without storing URLs.
    pub fn sample(&mut self, endpoint: &str, block: Option<&ObservedBlock>) {
        if self.samples.len() >= SAMPLE_LIMIT {
            self.samples_truncated = true;
            return;
        }
        self.samples.push(HeadSample {
            endpoint: endpoint.into(),
            elapsed_ms: self.origin.elapsed().as_millis() as u64,
            number: block.map(|value| value.number),
            timestamp: block.map(|value| value.timestamp),
            hash: block.map(|value| value.hash.clone()),
        });
    }

    /// Records a successful block observation.
    pub fn success(&mut self, endpoint: &str, block: &ObservedBlock) {
        self.count += 1;
        self.sample(endpoint, Some(block));
    }

    /// Records a failed observation and its sampling gap.
    pub fn failure(&mut self, endpoint: &str) {
        self.errors += 1;
        self.sample(endpoint, None);
    }

    /// Records a fatal RPC gap that prevents complete evaluation.
    pub fn infrastructure_failure(&mut self, endpoint: &str) {
        self.failure(endpoint);
        self.infrastructure_error = true;
    }
}

/// Bounded JSON-RPC observation client used by the runner.
#[derive(Debug)]
pub struct RpcObserver {
    client: Client,
    poll: Duration,
    request_timeout: Duration,
}

impl RpcObserver {
    /// Creates an observer with per-request and polling bounds.
    pub fn new(request_timeout: Duration, poll: Duration) -> Result<Self> {
        let client =
            Client::builder().redirect(Policy::none()).pool_max_idle_per_host(2).build()?;
        Ok(Self { client, poll, request_timeout })
    }

    /// Returns the endpoint chain identifier before `deadline`.
    pub async fn chain_id(&self, url: &str, deadline: Instant) -> Result<u64> {
        let value = self.rpc(url, "eth_chainId", json!([]), deadline).await?;
        Self::hex(Some(&value))
    }

    /// Returns a validated block selected by tag or quantity before `deadline`.
    pub async fn block(&self, url: &str, tag: &str, deadline: Instant) -> Result<ObservedBlock> {
        let value = self.rpc(url, "eth_getBlockByNumber", json!([tag, false]), deadline).await?;
        if value.is_null() {
            bail!("{tag} block is absent");
        }
        let block = ObservedBlock {
            number: Self::hex(value.get("number"))?,
            timestamp: Self::hex(value.get("timestamp"))?,
            hash: Self::hash(value.get("hash"))?,
        };
        if tag.starts_with("0x") && block.number != Self::hex(Some(&Value::String(tag.into())))? {
            bail!("RPC returned block {} for requested height {tag}", block.number);
        }
        Ok(block)
    }

    /// Executes one check within both its own and the scenario deadline.
    pub async fn run(
        &self,
        check: &AcceptanceCheck,
        endpoints: &BTreeMap<String, String>,
        samples: &mut Vec<HeadSample>,
        origin: Instant,
        scenario_deadline: Instant,
    ) -> CheckResult {
        let started = Instant::now();
        let deadline = (started + check.timeout()).min(scenario_deadline);
        let mut state = ObservationState::new(json!({ "check": check.kind() }), samples, origin);
        let evaluation = self.evaluate(check, endpoints, deadline, &mut state).await;
        let (status, message) = match evaluation {
            Ok(message) => (Status::Passed, message),
            Err(error) if state.infrastructure_error => (Status::Error, error.to_string()),
            Err(error) => (Status::Failed, error.to_string()),
        };
        if state.samples_truncated {
            state.observed["samples_truncated_at"] = json!(SAMPLE_LIMIT);
        }
        CheckResult {
            id: check.id().into(),
            kind: check.kind().into(),
            status,
            duration_ms: started.elapsed().as_millis() as u64,
            expected: state.expected,
            observed: state.observed,
            message,
            next_step: if status == Status::Passed {
                "no action required"
            } else {
                "inspect bounded Compose logs and endpoint health"
            }
            .into(),
            samples: state.count,
            rpc_errors: state.errors,
            evidence: Vec::new(),
        }
    }

    /// Dispatches a configured check to its observation algorithm.
    pub async fn evaluate(
        &self,
        check: &AcceptanceCheck,
        endpoints: &BTreeMap<String, String>,
        deadline: Instant,
        state: &mut ObservationState<'_>,
    ) -> Result<String> {
        match check {
            AcceptanceCheck::Contract { .. }
            | AcceptanceCheck::Transaction { .. }
            | AcceptanceCheck::Runtime { .. }
            | AcceptanceCheck::GlamsterdamBlobTransfers { .. } => {
                bail!("protocol check must be dispatched by the acceptance runner")
            }
            AcceptanceCheck::ChainId { endpoint, expected, .. } => {
                state.expected = json!(expected);
                match self.chain_id(Self::endpoint(endpoints, endpoint)?, deadline).await {
                    Ok(actual) => {
                        state.count += 1;
                        state.observed = json!(actual);
                        if actual == *expected {
                            Ok("chain identity matched".into())
                        } else {
                            bail!("chain id mismatch: expected {expected}, observed {actual}")
                        }
                    }
                    Err(error) => {
                        state.errors += 1;
                        state.infrastructure_error = true;
                        Err(error)
                    }
                }
            }
            AcceptanceCheck::HeadProgress { endpoint, minimum_blocks, .. }
            | AcceptanceCheck::SafeHeadProgress { endpoint, minimum_blocks, .. } => {
                state.expected = json!({ "minimum_blocks": minimum_blocks });
                let tag = if matches!(check, AcceptanceCheck::SafeHeadProgress { .. }) {
                    "safe"
                } else {
                    "latest"
                };
                self.progress(
                    Self::endpoint(endpoints, endpoint)?,
                    endpoint,
                    tag,
                    *minimum_blocks,
                    deadline,
                    state,
                )
                .await
            }
            AcceptanceCheck::HeadsConverge { endpoints: roles, head, max_lag_blocks, .. } => {
                state.expected = json!({
                    "max_lag_blocks": max_lag_blocks,
                    "common_height_hash": true,
                });
                self.converge(endpoints, roles, head, *max_lag_blocks, deadline, state).await
            }
            AcceptanceCheck::HeadFresh { endpoint, maximum_age, duration, .. } => {
                state.expected = json!({
                    "maximum_age_seconds": maximum_age.0.as_secs(),
                    "duration_ms": duration.0.as_millis(),
                });
                self.fresh(
                    Self::endpoint(endpoints, endpoint)?,
                    endpoint,
                    maximum_age.0,
                    duration.0,
                    deadline,
                    state,
                )
                .await
            }
            AcceptanceCheck::HeadsHealthy {
                endpoints: roles,
                warmup,
                duration,
                minimum_blocks,
                maximum_age,
                max_lag_blocks,
                ..
            } => {
                state.expected = json!({
                    "roles": roles,
                    "warmup_ms": warmup.0.as_millis(),
                    "duration_ms": duration.0.as_millis(),
                    "minimum_blocks_per_node": minimum_blocks,
                    "maximum_age_seconds": maximum_age.0.as_secs(),
                    "max_lag_blocks": max_lag_blocks,
                    "guarantee": "sampled health window; not proof of zero restarts between samples"
                });
                self.healthy(
                    endpoints,
                    roles,
                    (warmup.0, duration.0, *minimum_blocks, maximum_age.0, *max_lag_blocks),
                    deadline,
                    state,
                )
                .await
            }
        }
    }

    /// Samples all L2 roles for a complete shared window and requires sustained health.
    pub async fn healthy(
        &self,
        map: &BTreeMap<String, String>,
        roles: &[String],
        settings: (Duration, Duration, u64, Duration, u64),
        deadline: Instant,
        state: &mut ObservationState<'_>,
    ) -> Result<String> {
        let (warmup, duration, minimum, maximum_age, max_lag) = settings;
        let observation_start =
            Instant::now().checked_add(warmup).ok_or_else(|| eyre!("health warmup overflow"))?;
        tokio::time::sleep_until(observation_start).await;
        let mut baselines = BTreeMap::new();
        let mut latest = BTreeMap::new();
        let mut worst_ages = BTreeMap::new();
        let mut worst_lag = 0;
        let mut rounds = 0_u64;
        let mut until = None;
        loop {
            let mut heads = Vec::with_capacity(roles.len());
            for role in roles {
                let block = match self.block(Self::endpoint(map, role)?, "latest", deadline).await {
                    Ok(block) => block,
                    Err(error) => {
                        state.infrastructure_failure(role);
                        state.observed = json!({
                            "completed_rounds": rounds,
                            "partial_round_role": role,
                            "baselines": baselines,
                            "latest": latest,
                            "gaps": state.errors,
                        });
                        return Err(error.wrap_err(format!("health sample unavailable for {role}")));
                    }
                };
                state.success(role, &block);
                baselines.entry(role.clone()).or_insert(block.number);
                latest.insert(role.clone(), block.number);
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                // Match the L2 gossip validator's five-second future timestamp allowance.
                if block.timestamp.saturating_sub(now) > 5 {
                    state.observed = json!({
                        "role": role,
                        "timestamp": block.timestamp,
                        "host_timestamp": now,
                        "maximum_future_seconds": 5,
                    });
                    bail!("{role} head timestamp exceeds the five-second future allowance");
                }
                let age = now.saturating_sub(block.timestamp);
                worst_ages
                    .entry(role.clone())
                    .and_modify(|value: &mut u64| *value = (*value).max(age))
                    .or_insert(age);
                if age > maximum_age.as_secs() {
                    state.observed = json!({
                        "completed_rounds": rounds,
                        "baselines": baselines,
                        "latest": latest,
                        "worst_age_seconds": worst_ages,
                        "worst_lag_blocks": worst_lag,
                        "gaps": state.errors,
                    });
                    bail!("{role} head age {age}s exceeds freshness limit");
                }
                heads.push((role, block));
            }
            let low = heads.iter().map(|(_, block)| block.number).min().unwrap_or_default();
            let high = heads.iter().map(|(_, block)| block.number).max().unwrap_or_default();
            let lag = high.saturating_sub(low);
            worst_lag = worst_lag.max(lag);
            if lag > max_lag {
                state.observed = json!({
                    "completed_rounds": rounds,
                    "baselines": baselines,
                    "latest": latest,
                    "worst_age_seconds": worst_ages,
                    "worst_lag_blocks": worst_lag,
                    "gaps": state.errors,
                });
                bail!("head lag {lag} blocks exceeds limit {max_lag}");
            }
            let mut common_hash: Option<String> = None;
            let mut compared = Vec::with_capacity(heads.len());
            for (role, sampled) in &heads {
                let historical = match self
                    .block(Self::endpoint(map, role)?, &format!("0x{low:x}"), deadline)
                    .await
                {
                    Ok(block) => block,
                    Err(error) => {
                        state.infrastructure_failure(role);
                        state.observed = json!({
                            "completed_rounds": rounds,
                            "partial_round_role": role,
                            "common_height": low,
                            "baselines": baselines,
                            "latest": latest,
                            "gaps": state.errors,
                        });
                        return Err(
                            error.wrap_err(format!("canonical hash unavailable for {role}"))
                        );
                    }
                };
                state.count += 1;
                let comparison = json!({
                    "role": role,
                    "height": low,
                    "sampled_height": sampled.number,
                    "sampled_hash": sampled.hash,
                    "canonical_hash": historical.hash,
                });
                if sampled.number == low && sampled.hash != historical.hash {
                    state.observed = json!({
                        "completed_rounds": rounds,
                        "baselines": baselines,
                        "latest": latest,
                        "canonical_comparisons": [comparison],
                        "gaps": state.errors,
                    });
                    bail!(
                        "{role} sampled hash {} disagrees with canonical hash {} at height {low}",
                        sampled.hash,
                        historical.hash
                    );
                }
                if common_hash.as_ref().is_some_and(|hash| hash != &historical.hash) {
                    compared.push(comparison);
                    state.observed = json!({
                        "completed_rounds": rounds,
                        "baselines": baselines,
                        "latest": latest,
                        "canonical_comparisons": compared,
                        "gaps": state.errors,
                    });
                    bail!("node canonical hashes disagree at common height {low}");
                }
                common_hash = Some(historical.hash.clone());
                compared.push(comparison);
            }
            rounds += 1;
            if until.is_none() {
                let end = Instant::now()
                    .checked_add(duration)
                    .ok_or_else(|| eyre!("health duration overflow"))?;
                if end >= deadline {
                    bail!(
                        "health window leaves no reserved final round RPC budget before deadline"
                    );
                }
                until = Some(end);
            }
            let until = until.expect("health window initialized after baseline round");
            let deltas: BTreeMap<_, _> = latest
                .iter()
                .map(|(role, number)| (role, number.saturating_sub(baselines[role])))
                .collect();
            state.observed = json!({
                "baselines": baselines,
                "latest": latest,
                "deltas": deltas,
                "worst_age_seconds": worst_ages,
                "worst_lag_blocks": worst_lag,
                "completed_rounds": rounds,
                "gaps": state.errors,
                "last_common_height": low,
            });
            if Instant::now() >= until {
                let stalled: Vec<_> = deltas
                    .iter()
                    .filter(|(_, delta)| **delta < minimum)
                    .map(|(role, delta)| format!("{role} advanced {delta}"))
                    .collect();
                if !stalled.is_empty() {
                    bail!(
                        "insufficient independent progress: {}; required {minimum} blocks per node",
                        stalled.join(", ")
                    );
                }
                return Ok("all nodes remained available, fresh, canonical, within lag, and advanced independently for the sampled window".into());
            }
            tokio::time::sleep(self.poll.min(until.saturating_duration_since(Instant::now())))
                .await;
        }
    }

    /// Observes a head until it advances by the configured delta.
    pub async fn progress(
        &self,
        url: &str,
        role: &str,
        tag: &str,
        minimum: u64,
        deadline: Instant,
        state: &mut ObservationState<'_>,
    ) -> Result<String> {
        let sample_role = if tag == "latest" { role.into() } else { format!("{role}:{tag}") };
        let first = loop {
            match self.block(url, tag, deadline).await {
                Ok(block) => {
                    state.success(&sample_role, &block);
                    break block;
                }
                Err(error) => {
                    state.failure(&sample_role);
                    if !self.wait(deadline).await {
                        state.infrastructure_error = true;
                        return Err(error);
                    }
                }
            }
        };
        state.observed = json!({
            "from": first.number,
            "last": first.number,
            "required_delta": minimum,
        });
        let mut observation_error = None;
        loop {
            if !self.wait(deadline).await {
                if let Some(error) = observation_error {
                    state.infrastructure_error = true;
                    return Err(error);
                }
                let delta = state.observed["delta"].as_u64().unwrap_or_default();
                bail!("head advanced {delta} blocks; required {minimum}");
            }
            match self.block(url, tag, deadline).await {
                Ok(block) => {
                    observation_error = None;
                    state.success(&sample_role, &block);
                    let delta = block.number.saturating_sub(first.number);
                    state.observed = json!({
                        "from": first.number,
                        "last": block.number,
                        "delta": delta,
                        "required_delta": minimum,
                    });
                    if block.number < first.number {
                        bail!("head reorged below the initial observation");
                    }
                    if delta >= minimum {
                        return Ok("head advanced".into());
                    }
                }
                Err(error) => {
                    state.failure(&sample_role);
                    observation_error = Some(error);
                }
            }
        }
    }

    /// Observes endpoint heads and verifies lag and common-height canonicality.
    pub async fn converge(
        &self,
        map: &BTreeMap<String, String>,
        roles: &[String],
        tag: &str,
        max_lag: u64,
        deadline: Instant,
        state: &mut ObservationState<'_>,
    ) -> Result<String> {
        loop {
            let mut heads = Vec::new();
            let mut observation_error = None;
            for role in roles {
                let sample_role =
                    if tag == "latest" { role.clone() } else { format!("{role}:{tag}") };
                match self.block(Self::endpoint(map, role)?, tag, deadline).await {
                    Ok(block) => {
                        state.success(&sample_role, &block);
                        heads.push((role, block));
                    }
                    Err(error) => {
                        state.failure(&sample_role);
                        observation_error = Some(error);
                    }
                }
            }
            if heads.len() == roles.len() {
                let low = heads.iter().map(|(_, block)| block.number).min().unwrap_or_default();
                let high = heads.iter().map(|(_, block)| block.number).max().unwrap_or_default();
                state.observed = json!({
                    "low": low,
                    "high": high,
                    "lag": high.saturating_sub(low),
                    "maximum_lag": max_lag,
                });
                if high.saturating_sub(low) <= max_lag {
                    let mut common: Option<String> = None;
                    let mut compared = Vec::new();
                    for (role, sampled) in &heads {
                        match self
                            .block(Self::endpoint(map, role)?, &format!("0x{low:x}"), deadline)
                            .await
                        {
                            Ok(block) => {
                                // Historical hash verification is not a new head observation.
                                state.count += 1;
                                if sampled.number == low && sampled.hash != block.hash {
                                    bail!(
                                        "endpoint {role} reorganized sampled block {low} during comparison"
                                    );
                                }
                                compared.push(json!({
                                    "endpoint": role,
                                    "hash": block.hash,
                                }));
                                if common.as_ref().is_some_and(|hash| hash != &block.hash) {
                                    state.observed["common_height"] = json!(low);
                                    state.observed["compared"] = json!(compared);
                                    bail!("heads disagree at common height {low}");
                                }
                                common = Some(block.hash);
                            }
                            Err(error) => {
                                state.errors += 1;
                                observation_error = Some(error);
                            }
                        }
                    }
                    if compared.len() == roles.len() {
                        state.observed["common_height"] = json!(low);
                        state.observed["hash"] = json!(common);
                        return Ok("heads converged on a common canonical block".into());
                    }
                }
            }
            if !self.wait(deadline).await {
                if let Some(error) = observation_error {
                    state.infrastructure_error = true;
                    return Err(error);
                }
                bail!("heads did not converge before deadline");
            }
        }
    }

    /// Requires every sample to be available and fresh for the complete window.
    /// Block age uses the host's Unix wall clock, while the observation window uses monotonic time.
    /// Keep the host clock synchronized: clock skew or backward adjustments can cause a fresh
    /// head to appear stale or in the future.
    pub async fn fresh(
        &self,
        url: &str,
        role: &str,
        maximum_age: Duration,
        duration: Duration,
        deadline: Instant,
        state: &mut ObservationState<'_>,
    ) -> Result<String> {
        let start = Instant::now();
        let Some(until) = start.checked_add(duration) else {
            bail!("freshness duration overflow");
        };
        if until > deadline {
            bail!("freshness duration exceeds remaining check deadline");
        }
        let mut oldest = 0;
        loop {
            match self.block(url, "latest", deadline).await {
                Ok(block) => {
                    state.success(role, &block);
                    let age = SystemTime::now()
                        .duration_since(UNIX_EPOCH)?
                        .as_secs()
                        .checked_sub(block.timestamp)
                        .ok_or_else(|| eyre!("head timestamp is in the future"))?;
                    oldest = oldest.max(age);
                    state.observed = json!({
                        "oldest_age_seconds": oldest,
                        "last_block": block.number,
                        "maximum_age_seconds": maximum_age.as_secs(),
                    });
                    if age > maximum_age.as_secs() {
                        bail!("head age {age}s exceeds freshness limit");
                    }
                }
                Err(error) => {
                    state.infrastructure_failure(role);
                    return Err(error.wrap_err("freshness sample unavailable"));
                }
            }
            if Instant::now() >= until {
                break;
            }
            tokio::time::sleep(self.poll.min(until.saturating_duration_since(Instant::now())))
                .await;
        }
        if state.count == 0 {
            bail!("freshness window produced no samples");
        }
        Ok("head remained fresh for the observation window".into())
    }

    /// Performs one strictly bounded and protocol-validated JSON-RPC request.
    pub async fn rpc(
        &self,
        url: &str,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| eyre!("observation deadline elapsed"))?;
        let limit = self.request_timeout.min(remaining);
        let operation = async {
            let sent = self
                .client
                .post(url)
                .json(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": method,
                    "params": params,
                }))
                .send()
                .await;
            let mut response = sent.map_err(|error| Self::transport(&error))?;
            if !response.status().is_success() {
                bail!("RPC HTTP status {}", response.status());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) =
                response.chunk().await.map_err(|error| Self::transport(&error))?
            {
                if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
                    bail!("RPC response exceeds 1 MiB");
                }
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| eyre!("RPC response is not valid JSON"))?;
            if value.get("jsonrpc") != Some(&json!("2.0")) {
                bail!("RPC response has invalid protocol version");
            }
            if value.get("id") != Some(&json!(1)) {
                bail!("RPC response has mismatched id");
            }
            let result = value.get("result");
            let error = value.get("error");
            match (result, error) {
                (Some(result), None) => Ok(result.clone()),
                (None, Some(error)) => bail!("RPC {method} returned an error: {error}"),
                _ => bail!("RPC response must contain exactly one of result or error"),
            }
        };
        tokio::time::timeout(limit, operation)
            .await
            .map_err(|_| eyre!("RPC request deadline elapsed"))?
    }

    /// Sleeps for one polling interval, returning false at the deadline.
    pub async fn wait(&self, deadline: Instant) -> bool {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { return false };
        tokio::time::sleep(self.poll.min(remaining)).await;
        Instant::now() < deadline
    }

    /// Resolves a logical role without exposing its URL in errors.
    pub fn endpoint<'a>(map: &'a BTreeMap<String, String>, role: &str) -> Result<&'a str> {
        map.get(role).map(String::as_str).ok_or_else(|| eyre!("endpoint {role} was not resolved"))
    }

    /// Parses a canonical JSON-RPC hexadecimal quantity.
    pub fn hex(value: Option<&Value>) -> Result<u64> {
        let text = value.and_then(Value::as_str).ok_or_else(|| eyre!("hex quantity missing"))?;
        let digits =
            text.strip_prefix("0x").ok_or_else(|| eyre!("hex quantity lacks 0x prefix"))?;
        if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
            bail!("non-canonical hex quantity");
        }
        u64::from_str_radix(digits, 16).map_err(|_| eyre!("invalid hex quantity"))
    }

    /// Validates and returns a lowercase-or-uppercase 32-byte hash.
    pub fn hash(value: Option<&Value>) -> Result<String> {
        let hash = value.and_then(Value::as_str).ok_or_else(|| eyre!("block hash missing"))?;
        if hash.len() != 66
            || !hash.starts_with("0x")
            || !hash[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("block hash is not 0x-prefixed 32-byte hex");
        }
        Ok(hash.into())
    }

    /// Converts a reqwest failure to a credential-free transport reason.
    pub fn transport(error: &reqwest::Error) -> eyre::Report {
        let reason = if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "connection failure"
        } else if error.is_body() {
            "response body failure"
        } else if error.is_request() {
            "request failure"
        } else {
            "transport failure"
        };
        eyre!("RPC {reason}")
    }
}

#[cfg(test)]
mod tests {
    //! A hand-rolled HTTP fake is required because these tests need an ordered
    //! cross-request response script, delayed bodies, and oversized wire data.

    use std::{
        collections::{BTreeMap, VecDeque},
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
        time::Instant,
    };

    use super::{ObservationState, RpcObserver};
    use crate::{AcceptanceCheck, Span, Status};

    // Socket scheduling is real time; ordinary assertions must not benchmark the CI host.
    // Tests of deadline expiry set a deliberately shorter deadline or a longer server delay.
    const RPC_BUDGET: Duration = Duration::from_secs(5);

    #[derive(Clone)]
    struct Reply {
        delay: Duration,
        body: String,
    }

    async fn server(replies: Vec<Reply>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fallback = replies
            .last()
            .cloned()
            .unwrap_or_else(|| Reply { delay: Duration::ZERO, body: rpc(Value::Null) });
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let replies = Arc::clone(&replies);
                let fallback = fallback.clone();
                tokio::spawn(async move {
                    let mut request = vec![0; 8192];
                    let _ = socket.read(&mut request).await;
                    let mut reply = replies.lock().await.pop_front().unwrap_or(fallback);
                    tokio::time::sleep(reply.delay).await;
                    // Numeric timestamps are test-only offsets resolved when the response is
                    // served, so CPU scheduling cannot accidentally make a healthy head stale.
                    if let Ok(mut body) = serde_json::from_str::<Value>(&reply.body)
                        && let Some(offset) = body["result"]["timestamp"].as_i64()
                    {
                        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                        body["result"]["timestamp"] =
                            json!(format!("0x{:x}", now.saturating_add_signed(offset)));
                        reply.body = body.to_string();
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reply.body.len(),
                        reply.body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{address}")
    }

    fn rpc(result: Value) -> String {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": result,
        })
        .to_string()
    }

    fn block(number: u64, hash: char) -> Value {
        json!({
            "number": format!("0x{number:x}"),
            "timestamp": 0,
            "hash": format!("0x{}", hash.to_string().repeat(64)),
        })
    }

    fn block_at(number: u64, hash: char, timestamp: u64) -> Value {
        json!({
            "number": format!("0x{number:x}"),
            "timestamp": format!("0x{timestamp:x}"),
            "hash": format!("0x{}", hash.to_string().repeat(64)),
        })
    }

    fn replies(values: &[Value]) -> Vec<Reply> {
        values
            .iter()
            .cloned()
            .map(|value| Reply { delay: Duration::ZERO, body: rpc(value) })
            .collect()
    }

    fn observer() -> RpcObserver {
        RpcObserver::new(RPC_BUDGET, Duration::from_millis(2)).unwrap()
    }

    fn progress(timeout: Duration, minimum: u64) -> AcceptanceCheck {
        AcceptanceCheck::HeadProgress {
            id: "p".into(),
            endpoint: "rpc".into(),
            minimum_blocks: minimum,
            timeout: Span(timeout),
            start: None,
        }
    }

    fn healthy(duration: Duration, maximum_age: Duration) -> AcceptanceCheck {
        AcceptanceCheck::HeadsHealthy {
            id: "health".into(),
            endpoints: vec!["builder".into(), "validator".into()],
            warmup: Span(Duration::ZERO),
            duration: Span(duration),
            minimum_blocks: 1,
            maximum_age: Span(maximum_age),
            max_lag_blocks: 1,
            timeout: Span(RPC_BUDGET),
            start: None,
        }
    }

    async fn run_health(
        left: Vec<Reply>,
        right: Vec<Reply>,
        duration: Duration,
        maximum_age: Duration,
    ) -> crate::CheckResult {
        let map = BTreeMap::from([
            ("builder".into(), server(left).await),
            ("validator".into(), server(right).await),
        ]);
        let mut samples = Vec::new();
        observer()
            .run(
                &healthy(duration, maximum_age),
                &map,
                &mut samples,
                Instant::now(),
                Instant::now() + RPC_BUDGET + Duration::from_secs(1),
            )
            .await
    }

    #[tokio::test]
    async fn progress_uses_delta_and_stalled_large_head_fails() {
        let url = server(replies(&[block(1_000_000, 'a'), block(1_000_001, 'b')])).await;
        let map = BTreeMap::from([("rpc".into(), url)]);
        let mut samples = Vec::new();
        let result = RpcObserver::new(RPC_BUDGET, Duration::from_millis(200))
            .unwrap()
            .run(
                &progress(Duration::from_secs(1), 2),
                &map,
                &mut samples,
                Instant::now(),
                Instant::now() + RPC_BUDGET,
            )
            .await;
        assert_eq!(result.status, Status::Failed);
        assert_eq!(result.observed["from"], 1_000_000);
        assert!(result.message.contains("head advanced 1 blocks"));

        let url = server(replies(&[block(9, 'a'), block(10, 'b'), block(11, 'c')])).await;
        let mut samples = Vec::new();
        let result = observer()
            .run(
                &progress(RPC_BUDGET, 2),
                &BTreeMap::from([("rpc".into(), url)]),
                &mut samples,
                Instant::now(),
                Instant::now() + RPC_BUDGET,
            )
            .await;
        assert_eq!(result.status, Status::Passed);
        assert_eq!(result.observed["delta"], 2);
    }

    #[tokio::test]
    async fn progress_distinguishes_outage_from_recovered_violation() {
        let result = run_progress(replies(&[block(10, 'a'), Value::Null])).await;
        assert_eq!(result.status, Status::Error);
        assert_eq!(result.samples, 1);

        // A single request consuming the remaining deadline is also infrastructure failure.
        let mut delayed = replies(&[block(10, 'a'), block(11, 'b')]);
        delayed[1].delay = RPC_BUDGET;
        let result = run_progress(delayed).await;
        assert_eq!(result.status, Status::Error);
        assert_eq!(result.samples, 1);
        assert_eq!(result.rpc_errors, 1);

        let result = run_progress(replies(&[block(10, 'a'), Value::Null, block(9, 'b')])).await;
        assert_eq!(result.status, Status::Failed);
        assert!(result.message.contains("reorged"));
        assert!(result.rpc_errors > 0);

        let result = run_progress(replies(&[block(10, 'a'), Value::Null, block(11, 'b')])).await;
        assert_eq!(result.status, Status::Passed);
        assert!(result.rpc_errors > 0);
    }

    async fn run_progress(script: Vec<Reply>) -> crate::CheckResult {
        let url = server(script).await;
        let mut samples = Vec::new();
        observer()
            .run(
                &progress(Duration::from_secs(1), 1),
                &BTreeMap::from([("rpc".into(), url)]),
                &mut samples,
                Instant::now(),
                Instant::now() + RPC_BUDGET,
            )
            .await
    }

    #[tokio::test]
    async fn convergence_compares_common_height_and_accepts_lag() {
        let left = server(replies(&[block(10, 'a'), block(10, 'a')])).await;
        let right = server(replies(&[block(10, 'b'), block(10, 'b')])).await;
        let roles = vec!["a".into(), "b".into()];
        let mut samples = Vec::new();
        let mut state = ObservationState::new(json!({}), &mut samples, Instant::now());
        let result = observer()
            .converge(
                &BTreeMap::from([("a".into(), left), ("b".into(), right)]),
                &roles,
                "latest",
                0,
                Instant::now() + RPC_BUDGET,
                &mut state,
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("disagree"));

        let left = server(replies(&[block(10, 'a'), block(10, 'a')])).await;
        let right = server(replies(&[block(12, 'c'), block(10, 'a')])).await;
        let mut samples = Vec::new();
        let mut state = ObservationState::new(json!({}), &mut samples, Instant::now());
        assert!(
            observer()
                .converge(
                    &BTreeMap::from([("a".into(), left), ("b".into(), right)]),
                    &roles,
                    "latest",
                    2,
                    Instant::now() + RPC_BUDGET,
                    &mut state
                )
                .await
                .is_ok()
        );
        assert_eq!(
            samples.iter().filter_map(|sample| sample.number).collect::<Vec<_>>(),
            vec![10, 12]
        );
    }

    #[tokio::test]
    async fn partial_convergence_and_freshness_observations_remain_infrastructure_errors() {
        // Exercise both unavailable heads and unavailable historical comparison blocks.
        for script in [vec![Value::Null], vec![block(10, 'a'), Value::Null]] {
            let left = server(replies(&script)).await;
            let right = server(replies(&[block(10, 'a')])).await;
            let check = AcceptanceCheck::HeadsConverge {
                id: "converge".into(),
                endpoints: vec!["a".into(), "b".into()],
                head: "latest".into(),
                max_lag_blocks: 0,
                timeout: Span(Duration::from_secs(1)),
                start: None,
            };
            let result = RpcObserver::new(RPC_BUDGET, Duration::from_secs(2))
                .unwrap()
                .run(
                    &check,
                    &BTreeMap::from([("a".into(), left), ("b".into(), right)]),
                    &mut Vec::new(),
                    Instant::now(),
                    Instant::now() + RPC_BUDGET,
                )
                .await;
            assert_eq!(result.status, Status::Error);
            assert!(result.samples > 0);
            assert_eq!(result.rpc_errors, 1);
        }

        let url = server(replies(&[block(10, 'a'), Value::Null])).await;
        let check = AcceptanceCheck::HeadFresh {
            id: "fresh".into(),
            endpoint: "rpc".into(),
            maximum_age: Span(RPC_BUDGET),
            duration: Span(Duration::from_secs(1)),
            timeout: Span(RPC_BUDGET),
            start: None,
        };
        let result = observer()
            .run(
                &check,
                &BTreeMap::from([("rpc".into(), url)]),
                &mut Vec::new(),
                Instant::now(),
                Instant::now() + RPC_BUDGET,
            )
            .await;
        assert_eq!(result.status, Status::Error);
        assert_eq!(result.samples, 1);
        assert_eq!(result.rpc_errors, 1);
    }

    #[tokio::test]
    async fn block_rejects_wrong_height_and_response_cap() {
        let url = server(replies(&[block(8, 'a')])).await;
        assert!(
            observer()
                .block(&url, "0x7", Instant::now() + RPC_BUDGET)
                .await
                .unwrap_err()
                .to_string()
                .contains("requested")
        );
        let huge = Reply { delay: Duration::ZERO, body: "x".repeat(1_048_577) };
        let url = server(vec![huge]).await;
        assert!(
            observer()
                .chain_id(&url, Instant::now() + RPC_BUDGET)
                .await
                .unwrap_err()
                .to_string()
                .contains("1 MiB")
        );
    }

    #[tokio::test]
    async fn freshness_requires_available_full_window() {
        let good = Reply { delay: Duration::ZERO, body: rpc(block(1, 'a')) };
        let url = server(vec![good.clone(), good.clone(), good.clone(), good.clone(), good]).await;
        let mut samples = Vec::new();
        let mut state = ObservationState::new(json!({}), &mut samples, Instant::now());
        assert!(
            observer()
                .fresh(
                    &url,
                    "rpc",
                    RPC_BUDGET,
                    Duration::from_millis(10),
                    Instant::now() + RPC_BUDGET,
                    &mut state
                )
                .await
                .is_ok()
        );

        let url = server(vec![]).await;
        let mut samples = Vec::new();
        let mut state = ObservationState::new(json!({}), &mut samples, Instant::now());
        assert!(
            observer()
                .fresh(
                    &url,
                    "rpc",
                    Duration::from_secs(1),
                    Duration::from_millis(10),
                    Instant::now() + RPC_BUDGET,
                    &mut state
                )
                .await
                .is_err()
        );
        assert_eq!(state.errors, 1);

        let mut samples = Vec::new();
        let mut state = ObservationState::new(json!({}), &mut samples, Instant::now());
        assert!(
            observer()
                .fresh(
                    "http://127.0.0.1:1",
                    "rpc",
                    Duration::from_secs(1),
                    Duration::from_millis(20),
                    Instant::now() + Duration::from_millis(5),
                    &mut state
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
    }

    #[tokio::test]
    async fn late_response_cannot_pass_and_connection_error_is_counted() {
        let url =
            server(vec![Reply { delay: Duration::from_millis(30), body: rpc(json!(1)) }]).await;
        assert!(
            observer().chain_id(&url, Instant::now() + Duration::from_millis(5)).await.is_err()
        );

        let mut samples = Vec::new();
        let result = observer()
            .run(
                &progress(Duration::from_secs(1), 1),
                &BTreeMap::from([("rpc".into(), "http://127.0.0.1:1".into())]),
                &mut samples,
                Instant::now(),
                Instant::now() + RPC_BUDGET,
            )
            .await;
        assert_eq!(result.status, Status::Error);
        assert!(result.rpc_errors > 0);
        assert!(samples.iter().all(|sample| sample.number.is_none()));
    }

    #[tokio::test]
    async fn health_rejects_late_failure_and_stalled_fresh_heads() {
        let late_left = replies(&[block(10, 'a'), block(10, 'a'), block(11, 'b'), block(11, 'b')]);
        let late_right = replies(&[block(10, 'a'), block(10, 'a'), block(11, 'b'), block(11, 'c')]);
        let result =
            run_health(late_left, late_right, Duration::from_millis(20), Duration::from_secs(2))
                .await;
        assert_eq!(result.status, Status::Failed);
        assert!(result.message.contains("sampled hash") || result.message.contains("disagree"));
        assert_eq!(result.observed["completed_rounds"], 1);
        assert_eq!(result.observed["canonical_comparisons"][0]["role"], "validator");
        assert_eq!(result.observed["canonical_comparisons"][0]["height"], 11);
        assert_eq!(
            result.observed["canonical_comparisons"][0]["sampled_hash"],
            format!("0x{}", "b".repeat(64))
        );
        assert_eq!(
            result.observed["canonical_comparisons"][0]["canonical_hash"],
            format!("0x{}", "c".repeat(64))
        );

        let static_blocks: Vec<_> = (0..20).map(|_| block(100, 'a')).collect();
        let result = run_health(
            replies(&static_blocks),
            replies(&static_blocks),
            Duration::from_millis(6),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(result.status, Status::Failed);
        assert!(result.message.contains("insufficient independent progress"));
    }

    #[tokio::test]
    async fn health_window_starts_after_the_baseline_round() {
        let delayed_baseline =
            |hash| Reply { delay: Duration::from_millis(6), body: rpc(block(10, hash)) };
        let mut left =
            vec![delayed_baseline('a'), Reply { delay: Duration::ZERO, body: rpc(block(10, 'a')) }];
        left.extend(replies(&vec![block(11, 'b'); 20]));
        let right = left.clone();
        let started = Instant::now();
        let result =
            run_health(left, right, Duration::from_millis(10), Duration::from_secs(2)).await;

        assert_eq!(result.status, Status::Passed);
        assert!(started.elapsed() >= Duration::from_millis(20));
        assert!(result.observed["completed_rounds"].as_u64().unwrap() >= 2);
    }

    #[tokio::test]
    async fn health_classifies_partial_unavailability_as_error() {
        let left = replies(&[block(10, 'a'), block(10, 'a'), block(11, 'b'), block(11, 'b')]);
        let right = replies(&[block(10, 'a'), block(10, 'a'), Value::Null]);
        let result =
            run_health(left, right, Duration::from_millis(8), Duration::from_secs(2)).await;
        assert_eq!(result.status, Status::Error);
        assert!(result.rpc_errors > 0);
        assert_eq!(result.observed["completed_rounds"], 1);
        assert_eq!(result.observed["partial_round_role"], "validator");
    }

    #[tokio::test]
    async fn health_enforces_freshness_and_request_deadline_edges() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let mut advancing = vec![block(10, 'a'); 2];
        advancing.extend(vec![block(11, 'b'); 20]);
        for block in &mut advancing {
            block["timestamp"] = json!(5);
        }
        let result = run_health(
            replies(&advancing),
            replies(&advancing),
            Duration::from_millis(4),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(result.status, Status::Passed);
        let future = replies(&[block_at(10, 'a', u64::MAX)]);
        let result =
            run_health(future.clone(), future, Duration::from_millis(4), Duration::from_secs(2))
                .await;
        assert_eq!(result.status, Status::Failed);
        assert!(result.message.contains("future allowance"));
        let stale = replies(&[block_at(10, 'a', now - 3)]);
        let result =
            run_health(stale.clone(), stale, Duration::from_millis(4), Duration::from_secs(2))
                .await;
        assert_eq!(result.status, Status::Failed);
        assert!(result.message.contains("freshness limit"));

        let delayed =
            vec![Reply { delay: RPC_BUDGET + Duration::from_secs(1), body: rpc(block(10, 'a')) }];
        let result =
            run_health(delayed.clone(), delayed, Duration::from_millis(4), Duration::from_secs(2))
                .await;
        assert_eq!(result.status, Status::Error);
        assert!(result.rpc_errors > 0);
    }
}
