use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Terminal outcome, shared by lifecycle stages and acceptance checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The evaluated contract held.
    Passed,
    /// An evaluated assertion failed.
    Failed,
    /// An infrastructure or checker error prevented evaluation.
    Error,
    /// A prerequisite prevented this check from running.
    Blocked,
    /// Execution was interrupted.
    Cancelled,
}

impl Status {
    /// Human-readable status, also usable as a fixed CSS class.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Result of a bounded lifecycle operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageResult {
    /// Stable stage name.
    pub id: String,
    /// Operation outcome.
    pub status: Status,
    /// Time spent in this stage.
    pub duration_ms: u64,
    /// Sanitized diagnostic explanation.
    pub message: String,
}

/// Evidence-backed outcome of one configured acceptance check.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckResult {
    /// Stable check identifier.
    pub id: String,
    /// Typed check kind.
    pub kind: String,
    /// Evaluated or incomplete outcome.
    pub status: Status,
    /// Elapsed observation duration.
    pub duration_ms: u64,
    /// Required threshold or predicate.
    pub expected: Value,
    /// Independently observed values.
    pub observed: Value,
    /// Factual summary, not inferred causality.
    pub message: String,
    /// Suggested next inspection step.
    pub next_step: String,
    /// Number of completed observations.
    pub samples: u64,
    /// Failed RPC observations, including recovered errors.
    pub rpc_errors: u64,
    /// Relative evidence paths inside the report bundle.
    pub evidence: Vec<String>,
}

/// One observed head or explicit sampling gap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadSample {
    /// Logical endpoint role, never a credential-bearing URL.
    pub endpoint: String,
    /// Monotonic offset from observation start.
    pub elapsed_ms: u64,
    /// Canonical block number, absent for a failed sample.
    pub number: Option<u64>,
    /// Block timestamp in Unix seconds.
    pub timestamp: Option<u64>,
    /// Canonical block hash.
    pub hash: Option<String>,
}

/// Resolved schedule and separately observed boundary crossing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkBoundary {
    /// Fork identifier.
    pub name: String,
    /// Chain containing the boundary.
    pub chain: String,
    /// Activation timestamp from verified chain configuration.
    pub activation_timestamp: u64,
    /// First observed active block, not proof of fork behavior.
    pub observed_block: Option<u64>,
    /// When the crossing was observed by the checker.
    pub observed_elapsed_ms: Option<u64>,
}

/// Complete or partial result for one scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioResult {
    /// Stable scenario ID.
    pub id: String,
    /// Aggregate scenario status; consumers must also validate the records.
    pub status: Status,
    /// Total scenario execution time.
    pub duration_ms: u64,
    /// Sanitized resolved configuration.
    pub config: Value,
    /// Stage outcomes, including cleanup.
    pub stages: Vec<StageResult>,
    /// Every expected check, including blocked checks.
    pub checks: Vec<CheckResult>,
    /// Bounded head observations used by charts.
    pub samples: Vec<HeadSample>,
    /// Verified fork schedules and observed crossings.
    pub forks: Vec<ForkBoundary>,
    /// Sanitized warnings and collection limitations.
    pub diagnostics: Vec<String>,
    /// Inert reproduction command for the tested revision.
    pub reproduction: String,
}

impl ScenarioResult {
    /// Derives outcome from all stages and checks; an empty scenario cannot pass.
    pub fn outcome(&self) -> Status {
        let statuses: Vec<_> = self
            .stages
            .iter()
            .map(|stage| stage.status)
            .chain(self.checks.iter().map(|check| check.status))
            .collect();
        for status in [Status::Error, Status::Cancelled, Status::Failed, Status::Blocked] {
            if statuses.contains(&status) {
                return status;
            }
        }
        if self.checks.is_empty() { Status::Error } else { Status::Passed }
    }
}

/// Versioned portable result; source of truth for HTML and Markdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    /// Result format version (currently one).
    pub schema_version: u32,
    /// Unique execution identity; CI should include its run attempt.
    pub run_id: String,
    /// Exact tested Git revision, or an explicit local label.
    pub tested_sha: String,
    /// Run start in Unix milliseconds.
    pub started_at_unix_ms: u64,
    /// Scenario results in this bundle.
    pub scenarios: Vec<ScenarioResult>,
}

impl RunResult {
    /// Whether all nonempty scenarios completed successfully.
    pub fn passed(&self) -> bool {
        self.schema_version == 1
            && !self.scenarios.is_empty()
            && self.scenarios.iter().all(|scenario| scenario.outcome() == Status::Passed)
    }
}
