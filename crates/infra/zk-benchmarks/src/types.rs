//! Public types for ZK proof benchmarks.

use std::time::Duration;

use alloy_primitives::B256;
use base_prover_service_protocol::{ExecutionStats, ZkBackend};
use serde::{Deserialize, Serialize};
use url::Url;

mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Duration::from_millis(u64::deserialize(deserializer)?))
    }
}

/// Runtime configuration for proving a completed load-test run.
#[derive(Clone, Debug)]
pub struct ZkBenchConfig {
    /// Proof backend.
    pub zk_backend: ZkBackend,
    /// Composite ZK artifact hash to request.
    pub zk_artifact_hash: B256,
    /// Rollup node RPC URL.
    pub rollup_rpc_url: Url,
    /// ZK prover RPC URL.
    pub prover_url: Url,
}

/// The single L2 block selected for proof.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZkBenchTarget {
    /// L2 block selected for proof.
    pub block: u64,
    /// Human-readable selection reason.
    pub reason: String,
}

/// Proof request summary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZkBenchProofOutcome {
    /// Proof backend.
    pub zk_backend: ZkBackend,
    /// ZK prover session ID.
    pub session_id: String,
    /// Prover start block number (the proven block's parent).
    pub start_block_number: u64,
    /// L1 head hash passed to the prover.
    pub l1_head: B256,
    /// Proof wall-clock duration (JSON: milliseconds).
    #[serde(with = "duration_millis")]
    pub proof_duration: Duration,
}

/// Completed ZK benchmark summary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZkBenchSummary {
    /// Block selected from the load-test run for proof.
    pub target: ZkBenchTarget,
    /// Proof summary.
    pub proof: ZkBenchProofOutcome,
    /// Dry-run execution stats, when the prover returned them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_stats: Option<ExecutionStats>,
}

impl ZkBenchSummary {
    /// Serializes the summary to pretty JSON.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::B256;
    use base_prover_service_protocol::ZkBackend;

    use super::{ZkBenchProofOutcome, ZkBenchSummary, ZkBenchTarget};

    #[test]
    fn summary_json_encodes_proof_duration_as_millis() {
        let summary = ZkBenchSummary {
            target: ZkBenchTarget { block: 11, reason: "fullest".to_string() },
            proof: ZkBenchProofOutcome {
                zk_backend: ZkBackend::DryRun,
                session_id: "session".to_string(),
                start_block_number: 10,
                l1_head: B256::ZERO,
                proof_duration: Duration::from_millis(1_250),
            },
            execution_stats: None,
        };

        let json = summary.to_json().unwrap();
        assert!(json.contains("\"proof_duration\": 1250"), "{json}");
        assert!(!json.contains("\"secs\""), "{json}");
    }
}
