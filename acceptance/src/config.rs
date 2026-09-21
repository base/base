use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Duration,
};

use eyre::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ContractCase, ForkBoundary, GlamsterdamCheck, RuntimeCase, TransactionCase};

const FORKS: [&str; 5] = ["azul", "beryl", "cobalt", "denim", "zenith"];
const MAX_ROLLUP_JSON_BYTES: u64 = 1024 * 1024;
const MAX_DESCRIPTION_BYTES: usize = 1024;
const MAX_CHECKS: usize = 100;

/// CI suite assigned to a scenario.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CiSuite {
    /// Runs for pull requests and extended invocations.
    Pr,
    /// Runs only for extended invocations by default.
    #[default]
    Extended,
}

/// Strict CI metadata for a scenario.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiConfig {
    /// Suite used by CI discovery.
    #[serde(default)]
    pub suite: CiSuite,
}

/// A human-readable, positive duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span(pub Duration);

impl Serialize for Span {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&humantime::format_duration(self.0).to_string())
    }
}

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        humantime::parse_duration(&value).map(Self).map_err(serde::de::Error::custom)
    }
}

/// Strict version-one scenario document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioConfig {
    /// Configuration schema version (currently one).
    pub schema_version: u32,
    /// Stable scenario identifier.
    pub id: String,
    /// Human-readable purpose.
    pub description: String,
    /// CI discovery metadata.
    #[serde(default)]
    pub ci: CiConfig,
    /// Overall execution deadline.
    #[serde(default = "ScenarioConfig::default_timeout")]
    pub timeout: Span,
    /// Devnet topology and chain settings.
    #[serde(default)]
    pub devnet: DevnetConfig,
    /// RPC readiness settings.
    #[serde(default)]
    pub readiness: ReadinessConfig,
    /// Assertions to execute.
    pub checks: Vec<AcceptanceCheck>,
}

impl ScenarioConfig {
    /// Returns the default overall scenario timeout.
    pub const fn default_timeout() -> Span {
        Span(Duration::from_secs(600))
    }

    /// Loads strict TOML and validates it without contacting Docker or the network.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = fs::read_to_string(path.as_ref())
            .wrap_err_with(|| format!("read {}", path.as_ref().display()))?;
        let config: Self = toml::from_str(&text).wrap_err("parse scenario TOML")?;
        config.validate()?;
        Ok(config)
    }

    /// Validates schema, schedules, endpoints and all observation bounds.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!("unsupported schema_version {}; expected 1", self.schema_version);
        }
        Self::validate_id(&self.id, "scenario")?;
        if self.description.trim().is_empty() {
            bail!("description must not be empty");
        }
        if self.description.len() > MAX_DESCRIPTION_BYTES {
            bail!("description must not exceed {MAX_DESCRIPTION_BYTES} bytes");
        }
        Self::validate_duration(self.timeout.0, "scenario timeout", 1, 3600)?;
        if self.devnet.topology != "single-sequencer" {
            bail!("unsupported topology {}; HA is not version-one", self.devnet.topology);
        }
        if self.devnet.l1.chain_id == 0 || self.devnet.l2.chain_id == 0 {
            bail!("chain IDs must be non-zero");
        }
        if self.devnet.l2.verifier_l1_confirmations > 10_000 {
            bail!("verifier_l1_confirmations must not exceed 10000");
        }
        Self::validate_duration(self.devnet.l1.slot_duration.0, "L1 slot duration", 1, 60)?;
        if self.devnet.l1.slot_duration.0.subsec_nanos() != 0 {
            bail!("L1 slot duration must use whole seconds");
        }
        if self.devnet.l1.validator_count == 0 {
            bail!("L1 validator_count must be non-zero")
        }
        match self.devnet.profile {
            DevnetProfile::Canonical => {
                if self.devnet.l1.forks.glamsterdam.is_some() {
                    bail!("Glamsterdam forks require profile = 'glamsterdam'")
                }
            }
            DevnetProfile::Glamsterdam => {
                if self.devnet.l1.validator_count != 64
                    || self.devnet.l1.slot_duration.0 != Duration::from_secs(6)
                {
                    bail!("Glamsterdam requires 64 validators and 6s slots")
                }
                let fork = self.devnet.l1.forks.glamsterdam.as_ref().ok_or_else(|| {
                    eyre::eyre!("Glamsterdam profile requires its L1 fork schedule")
                })?;
                if fork.activation_epoch == 0 {
                    bail!("Glamsterdam activation_epoch must be non-zero")
                }
            }
        }
        Self::validate_duration_against(
            self.readiness.timeout.0,
            "readiness timeout",
            Duration::from_secs(1),
            self.timeout.0,
        )?;
        Self::validate_duration(self.readiness.request_timeout.0, "request timeout", 1, 30)?;
        Self::validate_duration(self.readiness.poll_interval.0, "poll interval", 1, 30)?;
        if self.readiness.request_timeout.0 > self.readiness.timeout.0 {
            bail!("request timeout exceeds readiness timeout");
        }
        if self.checks.is_empty() {
            bail!("at least one check is required");
        }
        if self.checks.len() > MAX_CHECKS {
            bail!("no more than {MAX_CHECKS} checks are allowed");
        }
        let protocol_checks = self
            .checks
            .iter()
            .filter(|check| matches!(check, AcceptanceCheck::GlamsterdamBlobTransfers { .. }))
            .count();
        if protocol_checks > 1 {
            bail!("at most one glamsterdam_blob_transfers check is allowed")
        }
        if protocol_checks == 1 && self.devnet.profile != DevnetProfile::Glamsterdam {
            bail!("glamsterdam_blob_transfers requires the Glamsterdam devnet profile")
        }
        if protocol_checks == 1
            && !matches!(
                self.checks.first(),
                Some(AcceptanceCheck::GlamsterdamBlobTransfers { .. })
            )
        {
            bail!("glamsterdam_blob_transfers must be the first check")
        }
        let mut ids = BTreeSet::new();
        let mut result_ids = BTreeSet::new();
        for check in &self.checks {
            Self::validate_id(check.id(), "check")?;
            for result_id in check.result_ids() {
                Self::validate_id(&result_id, "result check")?;
                if !result_ids.insert(result_id.to_ascii_lowercase()) {
                    bail!("duplicate result id {result_id}")
                }
            }
            if !ids.insert(check.id().to_ascii_lowercase()) {
                bail!("duplicate check id {}", check.id());
            }
            check.validate(self)?;
        }
        let mut previous = None;
        let mut prerequisite_disabled = false;
        for name in FORKS {
            let activation = self
                .devnet
                .l2
                .forks
                .get(name)
                .ok_or_else(|| eyre::eyre!("missing L2 fork {name}"))?;
            if let ForkActivation::Disabled { disabled: false } = activation {
                bail!("fork {name}: disabled must be true");
            }
            if let ForkActivation::AtBlock { at_block } = activation {
                if prerequisite_disabled {
                    bail!("enabled fork {name} has a disabled prerequisite");
                }
                if let Some(prior) = previous
                    && *at_block < prior
                {
                    bail!("fork {name} activates before its prerequisite");
                }
                if name == "zenith" {
                    let denim = self.devnet.l2.forks["denim"]
                        .block()
                        .ok_or_else(|| eyre::eyre!("Zenith requires Denim"))?;
                    if (at_block - denim) % 5 != 0 {
                        bail!("post-Denim fork offsets must be divisible by five");
                    }
                }
                previous = Some(*at_block);
            } else {
                prerequisite_disabled = true;
            }
        }
        for name in self.devnet.l2.forks.keys() {
            if !FORKS.contains(&name.as_str()) {
                bail!("unsupported L2 fork {name}");
            }
        }
        Ok(())
    }

    /// Resolves and verifies configured fork activation timestamps from generated rollup JSON.
    pub fn verified_forks(&self, rollup_path: &Path) -> Result<Vec<ForkBoundary>> {
        self.validate()?;
        let metadata = fs::metadata(rollup_path)?;
        if metadata.len() > MAX_ROLLUP_JSON_BYTES {
            bail!("rollup JSON exceeds {MAX_ROLLUP_JSON_BYTES} bytes");
        }
        let value: serde_json::Value = serde_json::from_slice(&fs::read(rollup_path)?)?;
        if value.get("l1_chain_id").and_then(serde_json::Value::as_u64)
            != Some(self.devnet.l1.chain_id)
        {
            bail!("generated L1 chain ID does not match scenario");
        }
        if value.get("l2_chain_id").and_then(serde_json::Value::as_u64)
            != Some(self.devnet.l2.chain_id)
        {
            bail!("generated L2 chain ID does not match scenario");
        }
        let genesis = value
            .pointer("/genesis/l2_time")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| eyre::eyre!("rollup genesis.l2_time is missing"))?;
        let generated = value
            .get("base")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| eyre::eyre!("rollup base schedule is missing"))?;
        let denim_block = self.devnet.l2.forks.get("denim").and_then(ForkActivation::block);
        let mut boundaries = Vec::new();
        for name in FORKS {
            let block = self.devnet.l2.forks.get(name).and_then(ForkActivation::block);
            let expected = block
                .map(|block| {
                    if block == 0 {
                        return Ok(0);
                    }
                    if let Some(denim) = denim_block.filter(|denim| block > *denim) {
                        genesis
                            .checked_add(
                                denim
                                    .checked_mul(2)
                                    .ok_or_else(|| eyre::eyre!("fork timestamp overflow"))?,
                            )
                            .and_then(|time| time.checked_add((block - denim) / 5))
                            .ok_or_else(|| eyre::eyre!("fork timestamp overflow"))
                    } else {
                        genesis
                            .checked_add(
                                block
                                    .checked_mul(2)
                                    .ok_or_else(|| eyre::eyre!("fork timestamp overflow"))?,
                            )
                            .ok_or_else(|| eyre::eyre!("fork timestamp overflow"))
                    }
                })
                .transpose()?;
            let actual = generated.get(name).and_then(serde_json::Value::as_u64);
            if actual != expected {
                bail!("generated {name} timestamp {actual:?} does not match expected {expected:?}");
            }
            if let Some(activation_timestamp) = actual {
                boundaries.push(ForkBoundary {
                    name: name.into(),
                    chain: "l2".into(),
                    activation_timestamp,
                    observed_block: None,
                    observed_elapsed_ms: None,
                });
            }
        }
        Ok(boundaries)
    }

    /// Validates a stable scenario or check identifier.
    pub fn validate_id(id: &str, what: &str) -> Result<()> {
        if id.is_empty()
            || id.len() > 63
            || !id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            bail!("invalid {what} id {id:?}");
        }
        Ok(())
    }

    /// Validates a duration against inclusive whole-second bounds.
    pub fn validate_duration(value: Duration, name: &str, min: u64, max: u64) -> Result<()> {
        Self::validate_duration_against(
            value,
            name,
            Duration::from_secs(min),
            Duration::from_secs(max),
        )
    }

    /// Validates a duration against inclusive duration bounds.
    pub fn validate_duration_against(
        value: Duration,
        name: &str,
        min: Duration,
        max: Duration,
    ) -> Result<()> {
        if value < min || value > max {
            bail!("{name} must be between {min:?} and {max:?}");
        }
        Ok(())
    }
}

/// Devnet topology and chain configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevnetConfig {
    /// Reviewed environment profile.
    #[serde(default)]
    pub profile: DevnetProfile,
    /// Topology name.
    #[serde(default = "DevnetConfig::default_topology")]
    pub topology: String,
    /// Layer-one settings.
    #[serde(default)]
    pub l1: L1Config,
    /// Layer-two settings.
    #[serde(default)]
    pub l2: L2Config,
}

impl DevnetConfig {
    /// Returns the default topology.
    pub fn default_topology() -> String {
        "single-sequencer".into()
    }
}

impl Default for DevnetConfig {
    fn default() -> Self {
        Self {
            profile: DevnetProfile::default(),
            topology: Self::default_topology(),
            l1: L1Config::default(),
            l2: L2Config::default(),
        }
    }
}

/// Reviewed devnet environment profile.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DevnetProfile {
    /// Existing canonical Compose environment.
    #[default]
    Canonical,
    /// Pinned Amsterdam/Gloas L1 with the canonical Rust blob batcher.
    Glamsterdam,
}

/// Layer-one settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct L1Config {
    /// Chain identifier.
    #[serde(default = "L1Config::default_chain_id")]
    pub chain_id: u64,
    /// Slot duration.
    #[serde(default = "L1Config::default_slot_duration")]
    pub slot_duration: Span,
    /// Number of genesis validators.
    #[serde(default = "L1Config::default_validator_count")]
    pub validator_count: u64,
    /// Layer-one fork settings.
    #[serde(default)]
    pub forks: L1Forks,
}

impl L1Config {
    /// Returns the default chain identifier.
    pub const fn default_chain_id() -> u64 {
        1337
    }

    /// Returns the default slot duration.
    pub const fn default_slot_duration() -> Span {
        Span(Duration::from_secs(12))
    }
    /// Returns the default validator count.
    pub const fn default_validator_count() -> u64 {
        1
    }
}

impl Default for L1Config {
    fn default() -> Self {
        Self {
            chain_id: Self::default_chain_id(),
            slot_duration: Self::default_slot_duration(),
            validator_count: Self::default_validator_count(),
            forks: L1Forks::default(),
        }
    }
}
/// Layer-one fork schedule.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct L1Forks {
    /// Matched Amsterdam execution and Gloas consensus activation.
    #[serde(default)]
    pub glamsterdam: Option<GlamsterdamFork>,
}
/// Matched Amsterdam execution and Gloas consensus activation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlamsterdamFork {
    /// Gloas activation epoch and corresponding Amsterdam epoch boundary.
    #[serde(default = "GlamsterdamFork::default_activation_epoch")]
    pub activation_epoch: u64,
}
impl GlamsterdamFork {
    /// Returns the reviewed activation epoch.
    pub const fn default_activation_epoch() -> u64 {
        8
    }
}

/// Layer-two settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct L2Config {
    /// Chain identifier.
    #[serde(default = "L2Config::default_chain_id")]
    pub chain_id: u64,
    /// Required layer-one confirmations.
    #[serde(default = "L2Config::default_confirmations")]
    pub verifier_l1_confirmations: u64,
    /// Fork activation schedule.
    #[serde(default = "L2Config::default_forks", deserialize_with = "L2Config::merge_forks")]
    pub forks: BTreeMap<String, ForkActivation>,
}

impl L2Config {
    /// Returns the default chain identifier.
    pub const fn default_chain_id() -> u64 {
        84_538_453
    }

    /// Returns the default confirmation count.
    pub const fn default_confirmations() -> u64 {
        15
    }
    /// Merges a partial fork schedule over defaults.
    pub fn merge_forks<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, ForkActivation>, D::Error> {
        let supplied = BTreeMap::<String, ForkActivation>::deserialize(deserializer)?;
        let mut forks = Self::default_forks();
        forks.extend(supplied);
        Ok(forks)
    }

    /// Returns the default fork schedule.
    pub fn default_forks() -> BTreeMap<String, ForkActivation> {
        [("azul", 20), ("beryl", 21), ("cobalt", 22), ("denim", 25)]
            .into_iter()
            .map(|(name, at_block)| (name.into(), ForkActivation::AtBlock { at_block }))
            .chain([(String::from("zenith"), ForkActivation::Disabled { disabled: true })])
            .collect()
    }
}

impl Default for L2Config {
    fn default() -> Self {
        Self {
            chain_id: Self::default_chain_id(),
            verifier_l1_confirmations: Self::default_confirmations(),
            forks: Self::default_forks(),
        }
    }
}

/// Fork activation setting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ForkActivation {
    /// Block activation.
    AtBlock {
        /// Activation block.
        at_block: u64,
    },
    /// Disabled activation.
    Disabled {
        /// Whether the fork is disabled.
        disabled: bool,
    },
}

impl ForkActivation {
    /// Returns the activation block when enabled.
    pub const fn block(&self) -> Option<u64> {
        match self {
            Self::AtBlock { at_block } => Some(*at_block),
            Self::Disabled { .. } => None,
        }
    }
}

/// Endpoint readiness settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessConfig {
    /// Overall readiness timeout.
    #[serde(default = "ReadinessConfig::default_timeout")]
    pub timeout: Span,
    /// Per-request timeout.
    #[serde(default = "ReadinessConfig::default_request_timeout")]
    pub request_timeout: Span,
    /// Poll interval.
    #[serde(default = "ReadinessConfig::default_poll_interval")]
    pub poll_interval: Span,
}

impl ReadinessConfig {
    /// Returns the default readiness timeout.
    pub const fn default_timeout() -> Span {
        Span(Duration::from_secs(240))
    }

    /// Returns the default request timeout.
    pub const fn default_request_timeout() -> Span {
        Span(Duration::from_secs(2))
    }

    /// Returns the default polling interval.
    pub const fn default_poll_interval() -> Span {
        Span(Duration::from_secs(1))
    }
}

impl Default for ReadinessConfig {
    fn default() -> Self {
        Self {
            timeout: Self::default_timeout(),
            request_timeout: Self::default_request_timeout(),
            poll_interval: Self::default_poll_interval(),
        }
    }
}

/// A fork-relative check start condition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckStart {
    /// Chain containing the fork.
    pub chain: String,
    /// Fork immediately before which to start.
    #[serde(default)]
    pub before_fork: Option<String>,
    /// Fork immediately after which to start.
    #[serde(default)]
    pub after_fork: Option<String>,
}

/// An acceptance assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AcceptanceCheck {
    /// Runs one isolated contract workload on the managed devnet.
    Contract {
        /// Check identifier.
        id: String,
        /// Reviewed contract behavior, not an arbitrary RPC program.
        case: ContractCase,
        /// Total workload budget including all submissions and observations.
        timeout: Span,
        /// Optional fork-relative observation window.
        #[serde(default)]
        start: Option<CheckStart>,
    },
    /// Runs one signed-transaction workload on the managed devnet.
    Transaction {
        /// Check identifier.
        id: String,
        /// Reviewed transaction behavior.
        case: TransactionCase,
        /// Total workload budget.
        timeout: Span,
        /// Optional fork-relative observation window.
        #[serde(default)]
        start: Option<CheckStart>,
    },
    /// Runs one deployment-runtime workload on the managed devnet.
    Runtime {
        /// Check identifier.
        id: String,
        /// Reviewed runtime behavior.
        case: RuntimeCase,
        /// Total workload budget.
        timeout: Span,
        /// Optional fork-relative observation window.
        #[serde(default)]
        start: Option<CheckStart>,
    },
    /// Verifies blob derivation and chain rules across L1 Glamsterdam.
    GlamsterdamBlobTransfers {
        /// Prefix for stable assertion result identifiers.
        id: String,
        /// Overall protocol-check timeout.
        timeout: Span,
    },
    /// Checks a chain identifier.
    ChainId {
        /// Check identifier.
        id: String,
        /// Endpoint name.
        endpoint: String,
        /// Expected identifier.
        expected: u64,
        /// Check timeout.
        timeout: Span,
        /// Optional start condition.
        #[serde(default)]
        start: Option<CheckStart>,
    },
    /// Checks head progress.
    HeadProgress {
        /// Check identifier.
        id: String,
        /// Endpoint name.
        endpoint: String,
        /// Minimum progress.
        minimum_blocks: u64,
        /// Check timeout.
        timeout: Span,
        /// Optional start condition.
        #[serde(default)]
        start: Option<CheckStart>,
    },
    /// Checks head convergence.
    HeadsConverge {
        /// Check identifier.
        id: String,
        /// Endpoint names.
        endpoints: Vec<String>,
        /// Head tag.
        #[serde(default = "AcceptanceCheck::default_head")]
        head: String,
        /// Maximum lag.
        max_lag_blocks: u64,
        /// Check timeout.
        timeout: Span,
        /// Optional start condition.
        #[serde(default)]
        start: Option<CheckStart>,
    },
    /// Checks safe-head progress.
    SafeHeadProgress {
        /// Check identifier.
        id: String,
        /// Endpoint name.
        endpoint: String,
        /// Minimum progress.
        minimum_blocks: u64,
        /// Check timeout.
        timeout: Span,
        /// Optional start condition.
        #[serde(default)]
        start: Option<CheckStart>,
    },
    /// Checks head freshness.
    HeadFresh {
        /// Check identifier.
        id: String,
        /// Endpoint name.
        endpoint: String,
        /// Maximum head age.
        maximum_age: Span,
        /// Observation duration.
        duration: Span,
        /// Check timeout.
        timeout: Span,
        /// Optional start condition.
        #[serde(default)]
        start: Option<CheckStart>,
    },
    /// Checks sustained health and progress across L2 node roles.
    HeadsHealthy {
        /// Check identifier.
        id: String,
        /// Distinct L2 endpoint roles.
        endpoints: Vec<String>,
        /// Delay before establishing baselines.
        #[serde(default = "AcceptanceCheck::default_warmup")]
        warmup: Span,
        /// Complete observation window after warmup.
        duration: Span,
        /// Minimum independent progress required from every node.
        minimum_blocks: u64,
        /// Maximum permitted head age.
        maximum_age: Span,
        /// Maximum permitted head lag.
        max_lag_blocks: u64,
        /// Check timeout, including warmup and observations.
        timeout: Span,
        /// Optional start condition.
        #[serde(default)]
        start: Option<CheckStart>,
    },
}

impl AcceptanceCheck {
    /// Returns the default head tag.
    pub fn default_head() -> String {
        "latest".into()
    }

    /// Returns the default zero warmup.
    pub const fn default_warmup() -> Span {
        Span(Duration::ZERO)
    }

    /// Returns the check identifier.
    pub fn id(&self) -> &str {
        match self {
            Self::Contract { id, .. }
            | Self::Transaction { id, .. }
            | Self::Runtime { id, .. }
            | Self::GlamsterdamBlobTransfers { id, .. }
            | Self::ChainId { id, .. }
            | Self::HeadProgress { id, .. }
            | Self::HeadsConverge { id, .. }
            | Self::SafeHeadProgress { id, .. }
            | Self::HeadFresh { id, .. }
            | Self::HeadsHealthy { id, .. } => id,
        }
    }

    /// Returns the check kind.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Contract { .. } => "contract",
            Self::Transaction { .. } => "transaction",
            Self::Runtime { .. } => "runtime",
            Self::GlamsterdamBlobTransfers { .. } => "glamsterdam_blob_transfers",
            Self::ChainId { .. } => "chain_id",
            Self::HeadProgress { .. } => "head_progress",
            Self::HeadsConverge { .. } => "heads_converge",
            Self::SafeHeadProgress { .. } => "safe_head_progress",
            Self::HeadFresh { .. } => "head_fresh",
            Self::HeadsHealthy { .. } => "heads_healthy",
        }
    }

    /// Returns the check timeout.
    pub const fn timeout(&self) -> Duration {
        match self {
            Self::Contract { timeout, .. }
            | Self::Transaction { timeout, .. }
            | Self::Runtime { timeout, .. }
            | Self::GlamsterdamBlobTransfers { timeout, .. }
            | Self::ChainId { timeout, .. }
            | Self::HeadProgress { timeout, .. }
            | Self::HeadsConverge { timeout, .. }
            | Self::SafeHeadProgress { timeout, .. }
            | Self::HeadFresh { timeout, .. }
            | Self::HeadsHealthy { timeout, .. } => timeout.0,
        }
    }

    /// Returns the optional start condition.
    pub const fn start(&self) -> Option<&CheckStart> {
        match self {
            Self::GlamsterdamBlobTransfers { .. } => None,
            Self::Contract { start, .. }
            | Self::Transaction { start, .. }
            | Self::Runtime { start, .. }
            | Self::ChainId { start, .. }
            | Self::HeadProgress { start, .. }
            | Self::HeadsConverge { start, .. }
            | Self::SafeHeadProgress { start, .. }
            | Self::HeadFresh { start, .. }
            | Self::HeadsHealthy { start, .. } => start.as_ref(),
        }
    }

    /// Validates the check against its containing scenario.
    pub fn validate(&self, scenario: &ScenarioConfig) -> Result<()> {
        ScenarioConfig::validate_duration_against(
            self.timeout(),
            "check timeout",
            Duration::from_secs(1),
            scenario.timeout.0,
        )?;
        let endpoints = match self {
            Self::Contract { case, .. } => {
                case.validate(scenario)?;
                case.required_roles().to_vec()
            }
            Self::Transaction { case, .. } => {
                case.validate(scenario)?;
                case.required_roles().to_vec()
            }
            Self::Runtime { case, .. } => {
                case.validate(scenario)?;
                case.required_roles().to_vec()
            }
            Self::GlamsterdamBlobTransfers { .. } => Vec::new(),
            Self::ChainId { endpoint, .. }
            | Self::HeadProgress { endpoint, .. }
            | Self::SafeHeadProgress { endpoint, .. }
            | Self::HeadFresh { endpoint, .. } => vec![endpoint.as_str()],
            Self::HeadsConverge { endpoints, head, .. } => {
                if endpoints.len() < 2 {
                    bail!("heads_converge requires two endpoints");
                }
                if !matches!(head.as_str(), "latest" | "safe" | "finalized") {
                    bail!("unsupported head tag {head}");
                }
                let unique: BTreeSet<_> = endpoints.iter().collect();
                if unique.len() != endpoints.len() {
                    bail!("heads_converge endpoints must be distinct");
                }
                endpoints.iter().map(String::as_str).collect()
            }
            Self::HeadsHealthy { endpoints, .. } => {
                if endpoints.len() < 2 {
                    bail!("heads_healthy requires at least two endpoints");
                }
                let unique: BTreeSet<_> = endpoints.iter().collect();
                if unique.len() != endpoints.len() {
                    bail!("heads_healthy endpoints must be distinct");
                }
                if endpoints.iter().any(|endpoint| endpoint == "l1") {
                    bail!("heads_healthy accepts only L2 endpoint roles");
                }
                endpoints.iter().map(String::as_str).collect()
            }
        };
        for endpoint in endpoints {
            if !matches!(
                endpoint,
                "l1" | "beacon"
                    | "builder"
                    | "builder-consensus"
                    | "builder-flashblocks"
                    | "builder-metrics"
                    | "validator"
                    | "validator-consensus"
                    | "validator-metrics"
                    | "rpc"
                    | "shadow"
            ) {
                bail!("unknown endpoint {endpoint}");
            }
        }
        if let Some(start) = self.start() {
            if (start.before_fork.is_some()) == (start.after_fork.is_some()) {
                bail!("start requires exactly one of before_fork/after_fork");
            }
            if start.chain != "l2" {
                bail!("configurable L1 forks (including Glamsterdam) are unsupported");
            }
            let fork = start.before_fork.as_ref().or(start.after_fork.as_ref()).unwrap();
            if scenario.devnet.l2.forks.get(fork).and_then(ForkActivation::block).is_none() {
                bail!("check references absent or disabled fork {fork}");
            }
        }
        if let Self::HeadFresh { duration, timeout, .. } = self {
            ScenarioConfig::validate_duration_against(
                duration.0,
                "freshness duration",
                Duration::from_secs(1),
                timeout.0,
            )?
        }
        if let Self::HeadsHealthy { warmup, duration, timeout, .. } = self {
            ScenarioConfig::validate_duration_against(
                duration.0,
                "health duration",
                Duration::from_secs(1),
                timeout.0,
            )?;
            if warmup.0.checked_add(duration.0).is_none_or(|total| total >= timeout.0) {
                bail!("heads_healthy timeout must exceed warmup plus duration for RPC budget");
            }
        }
        match self {
            Self::HeadProgress { minimum_blocks: 0, .. }
            | Self::SafeHeadProgress { minimum_blocks: 0, .. }
            | Self::HeadsHealthy { minimum_blocks: 0, .. } => {
                bail!("minimum_blocks must be non-zero");
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether execution requires resources owned by this invocation.
    pub const fn requires_managed(&self) -> bool {
        matches!(
            self,
            Self::Contract { .. }
                | Self::Transaction { .. }
                | Self::Runtime { .. }
                | Self::GlamsterdamBlobTransfers { .. }
        )
    }

    /// Expands this configured check into stable portable result identifiers.
    pub fn result_ids(&self) -> Vec<String> {
        match self {
            Self::GlamsterdamBlobTransfers { id, .. } => {
                GlamsterdamCheck::STAGES.iter().map(|stage| format!("{id}-{stage}")).collect()
            }
            _ => vec![self.id().into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_SCENARIO: &str = r#"
schema_version = 1
id = "smoke"
description = "x"

[[checks]]
id = "identity"
kind = "chain_id"
endpoint = "builder"
expected = 84538453
timeout = "5s"
"#;

    #[test]
    fn rejects_unknown_and_empty() {
        assert!(
            toml::from_str::<ScenarioConfig>(
                "schema_version=1\nid='x'\ndescription='x'\nchecks=[]\nextra=1"
            )
            .is_err()
        );
    }

    #[test]
    fn validates_minimal() {
        let config: ScenarioConfig = toml::from_str(MINIMAL_SCENARIO).unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.devnet.profile, DevnetProfile::Canonical);
        assert_eq!(config.devnet.l1.validator_count, 1);
        assert!(config.devnet.l1.forks.glamsterdam.is_none());
    }

    #[test]
    fn validates_only_the_reviewed_glamsterdam_profile() {
        let source = MINIMAL_SCENARIO.replace(
            "[[checks]]",
            r#"[devnet]
profile = "glamsterdam"

[devnet.l1]
validator_count = 64
slot_duration = "6s"

[devnet.l1.forks]
glamsterdam = {}

[[checks]]"#,
        );
        let config: ScenarioConfig = toml::from_str(&source).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.devnet.l1.forks.glamsterdam.unwrap().activation_epoch,
            GlamsterdamFork::default_activation_epoch()
        );

        for invalid in [
            source.replace("validator_count = 64", "validator_count = 63"),
            source.replace("slot_duration = \"6s\"", "slot_duration = \"12s\""),
            source.replace("glamsterdam = {}", "glamsterdam = { activation_epoch = 0 }"),
        ] {
            assert!(toml::from_str::<ScenarioConfig>(&invalid).unwrap().validate().is_err());
        }
    }

    #[test]
    fn protocol_check_expands_ids_and_must_be_unique_and_first() {
        let source = r#"
schema_version = 1
id = "glamsterdam"
description = "protocol"

[devnet]
profile = "glamsterdam"
[devnet.l1]
validator_count = 64
slot_duration = "6s"
[devnet.l1.forks]
glamsterdam = {}

[[checks]]
id = "glam"
kind = "glamsterdam_blob_transfers"
timeout = "500s"

[[checks]]
id = "health"
kind = "chain_id"
endpoint = "builder"
expected = 84538453
timeout = "5s"
"#;
        let config: ScenarioConfig = toml::from_str(source).unwrap();
        config.validate().unwrap();
        assert_eq!(config.checks[0].result_ids().len(), 11);
        assert_eq!(config.checks[0].result_ids()[0], "glam-schedule");
        assert_eq!(config.checks[1].result_ids(), ["health"]);

        let colliding = source.replace("id = \"health\"", "id = \"glam-schedule\"");
        assert!(toml::from_str::<ScenarioConfig>(&colliding).unwrap().validate().is_err());
        let mut wrong_profile = config;
        wrong_profile.devnet = DevnetConfig::default();
        assert!(wrong_profile.validate().is_err());

        let reversed = source.replacen(
            "[[checks]]\nid = \"glam\"",
            r#"[[checks]]
id = "later"
kind = "chain_id"
endpoint = "builder"
expected = 84538453
timeout = "5s"

[[checks]]
id = "glam""#,
            1,
        );
        assert!(toml::from_str::<ScenarioConfig>(&reversed).unwrap().validate().is_err());
    }

    #[test]
    fn partial_forks_retain_devnet_defaults() {
        let config: ScenarioConfig = toml::from_str(&format!(
            "{MINIMAL_SCENARIO}\n[devnet.l2.forks]\ndenim = {{ at_block = 30 }}\n"
        ))
        .unwrap();
        assert_eq!(config.devnet.l2.forks["azul"].block(), Some(20));
        assert_eq!(config.devnet.l2.forks["denim"].block(), Some(30));
    }

    #[test]
    fn rejects_unknown_fork_and_false_disabled() {
        let unknown: ScenarioConfig = toml::from_str(&format!(
            "{MINIMAL_SCENARIO}\n[devnet.l2.forks]\nwat = {{ at_block = 30 }}\n"
        ))
        .unwrap();
        assert!(unknown.validate().is_err());
        let false_disabled: ScenarioConfig = toml::from_str(&format!(
            "{MINIMAL_SCENARIO}\n[devnet.l2.forks]\nzenith = {{ disabled = false }}\n"
        ))
        .unwrap();
        assert!(false_disabled.validate().is_err());
    }

    #[test]
    fn zenith_requires_enabled_ordered_denim() {
        let mut config = ScenarioConfig::load("scenarios/smoke.toml").unwrap();
        config.devnet.l2.forks.insert("zenith".into(), ForkActivation::AtBlock { at_block: 24 });
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "fork zenith activates before its prerequisite"
        );

        for at_block in [25, 30] {
            config.devnet.l2.forks.insert("zenith".into(), ForkActivation::AtBlock { at_block });
            config.validate().unwrap();
        }
        config.devnet.l2.forks.insert("zenith".into(), ForkActivation::AtBlock { at_block: 26 });
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "post-Denim fork offsets must be divisible by five"
        );

        config.devnet.l2.forks.insert("denim".into(), ForkActivation::Disabled { disabled: true });
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "enabled fork zenith has a disabled prerequisite"
        );
    }

    #[test]
    fn rejects_wrong_generated_schedule() {
        let config: ScenarioConfig = toml::from_str(MINIMAL_SCENARIO).unwrap();
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            temp.path(),
            r#"{
                "l1_chain_id": 1337,
                "l2_chain_id": 84538453,
                "genesis": { "l2_time": 100 },
                "base": { "azul": 141, "beryl": 142, "cobalt": 144, "denim": 150 }
            }"#,
        )
        .unwrap();
        assert_eq!(
            config.verified_forks(temp.path()).unwrap_err().to_string(),
            "generated azul timestamp Some(141) does not match expected Some(140)"
        );
    }

    #[test]
    fn verifies_post_denim_slot_offset() {
        let mut config: ScenarioConfig = toml::from_str(MINIMAL_SCENARIO).unwrap();
        config.devnet.l2.forks.insert("zenith".into(), ForkActivation::AtBlock { at_block: 30 });
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            temp.path(),
            r#"{
                "l1_chain_id": 1337,
                "l2_chain_id": 84538453,
                "genesis": { "l2_time": 100 },
                "base": {
                    "azul": 140,
                    "beryl": 142,
                    "cobalt": 144,
                    "denim": 150,
                    "zenith": 151
                }
            }"#,
        )
        .unwrap();
        let forks = config.verified_forks(temp.path()).unwrap();
        assert_eq!(forks.last().unwrap().activation_timestamp, 151);
    }
}
