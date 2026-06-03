//! YAML and runtime configuration for ZK benchmarks.

use std::{fs, path::Path};

use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{Profiles, ProofConfig, ProofMode};

/// Top-level ZK benchmark scenario configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    /// Benchmark name.
    pub name: String,
    /// Runtime profile name. Defaults to `devnet`.
    #[serde(default = "BenchmarkConfig::default_profile")]
    pub profile: String,
    /// Workload scenario configuration.
    pub workload: WorkloadConfig,
    /// Proof configuration.
    #[serde(default)]
    pub proof: ProofConfig,
}

impl BenchmarkConfig {
    /// Returns the default profile name.
    pub fn default_profile() -> String {
        "devnet".to_string()
    }

    /// Loads a benchmark config from YAML.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read benchmark config {}", path.display()))?;
        Self::from_yaml(&contents)
    }

    /// Parses a benchmark config from YAML.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let config: Self = serde_yaml::from_str(yaml).wrap_err("failed to parse benchmark YAML")?;
        config.validate()?;
        Ok(config)
    }

    /// Validates semantic config requirements.
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.name.trim().is_empty(), "benchmark name must not be empty");
        self.proof.validate()
    }

    /// Resolves the runtime profile, applying CLI overrides.
    pub fn resolve_profile(
        &self,
        overrides: &BenchmarkConfigOverrides,
    ) -> Result<BenchmarkProfile> {
        let mut profile = Profiles::get(&self.profile)
            .ok_or_else(|| eyre::eyre!("unknown benchmark profile: {}", self.profile))?;
        if let Some(url) = &overrides.l2_rpc_url {
            profile.l2_rpc_url = url.clone();
        }
        if let Some(url) = &overrides.rollup_rpc_url {
            profile.rollup_rpc_url = url.clone();
        }
        if let Some(url) = &overrides.zk_prover_url {
            profile.zk_prover_url = url.clone();
        }
        if let Some(chain_id) = overrides.l2_chain_id {
            profile.l2_chain_id = chain_id;
        }
        Ok(profile)
    }

    /// Resolves proof configuration, applying CLI overrides.
    pub fn resolve_proof(&self, overrides: &BenchmarkConfigOverrides) -> ProofConfig {
        let mut proof = self.proof.clone();
        if let Some(mode) = overrides.proof_mode {
            proof.mode = mode;
        }
        proof
    }
}

/// CLI-provided config overrides.
#[derive(Clone, Debug, Default)]
pub struct BenchmarkConfigOverrides {
    /// Optional L2 RPC override.
    pub l2_rpc_url: Option<Url>,
    /// Optional rollup RPC override.
    pub rollup_rpc_url: Option<Url>,
    /// Optional ZK prover RPC override.
    pub zk_prover_url: Option<Url>,
    /// Optional L2 chain ID override.
    pub l2_chain_id: Option<u64>,
    /// Optional proof mode override.
    pub proof_mode: Option<ProofMode>,
}

/// Resolved runtime endpoint profile.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkProfile {
    /// Profile name.
    pub name: String,
    /// L2 execution RPC URL.
    pub l2_rpc_url: Url,
    /// Rollup RPC URL.
    pub rollup_rpc_url: Url,
    /// ZK prover RPC URL.
    pub zk_prover_url: Url,
    /// L2 chain ID.
    pub l2_chain_id: u64,
}

/// Workload scenario configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkloadConfig {
    /// Existing deterministic B20 sequence benchmark.
    B20Sequence(B20SequenceWorkloadConfig),
}

/// B20 sequence workload config.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct B20SequenceWorkloadConfig {
    /// Devnet account index used as token admin.
    pub admin_account: u32,
    /// Devnet account index used as spender.
    pub spender_account: u32,
    /// Devnet account index used as transfer recipient.
    pub recipient_account: u32,
}

impl Default for B20SequenceWorkloadConfig {
    fn default() -> Self {
        Self { admin_account: 5, spender_account: 7, recipient_account: 6 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_b20_sequence_uses_devnet_defaults() {
        let config = BenchmarkConfig::from_yaml(
            r#"
name: b20-sequence
workload:
  type: b20_sequence
"#,
        )
        .unwrap();

        let profile = config.resolve_profile(&BenchmarkConfigOverrides::default()).unwrap();
        assert_eq!(profile.name, "devnet");
        assert_eq!(profile.l2_chain_id, 84_538_453);
        assert_eq!(profile.l2_rpc_url.as_str(), "http://localhost:8645/");
    }

    #[test]
    fn proof_mode_override_replaces_config() {
        let config = BenchmarkConfig::from_yaml(
            r#"
name: b20-sequence
proof:
  mode: dry_run
workload:
  type: b20_sequence
"#,
        )
        .unwrap();
        let proof = config.resolve_proof(&BenchmarkConfigOverrides {
            proof_mode: Some(ProofMode::Cluster),
            ..BenchmarkConfigOverrides::default()
        });

        assert_eq!(proof.mode, ProofMode::Cluster);
    }
}
