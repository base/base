//! Benchmark configuration types and matrix expansion.

use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::BenchmarkError;

/// Top-level benchmark configuration loaded from YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    /// Human-readable name for this benchmark suite.
    pub name: String,
    /// Optional long description.
    pub description: Option<String>,
    /// Target block time in milliseconds.
    pub block_time_ms: u64,
    /// Number of blocks to produce per run.
    pub num_blocks: u64,
    /// Optional per-block gas limit override (default 30M).
    pub gas_limit: Option<u64>,
    /// Path to the OP-Stack rollup config JSON.
    pub rollup_config: Option<PathBuf>,
    /// Number of parallel transaction batches to send.
    pub parallel_tx_batches: Option<u64>,
    /// Flashblocks replay configuration.
    pub flashblocks: Option<FlashblocksConfig>,
    /// Test definitions to run (each expanded by variables).
    pub benchmarks: Vec<BenchmarkDefinition>,
}

impl BenchmarkConfig {
    /// Expand all benchmark definitions into a flat list of [`TestRun`]s via
    /// cartesian product of each definition's variables. Returns an error if
    /// the expansion would exceed 100 runs.
    pub fn expand(&self) -> Result<Vec<TestRun>, BenchmarkError> {
        let mut runs = Vec::new();
        for definition in &self.benchmarks {
            let expanded = expand_variables(&definition.variables);
            for params in expanded {
                runs.push(TestRun {
                    id: crate::output::random_id(),
                    params,
                    definition: definition.clone(),
                });
            }
        }
        if runs.len() > 100 {
            return Err(BenchmarkError::Config(format!(
                "matrix expansion produced {} test runs, maximum is 100",
                runs.len()
            )));
        }
        Ok(runs)
    }
}

/// Flashblocks configuration for block-time-aware replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashblocksConfig {
    /// Target flashblock interval in milliseconds.
    pub block_time_ms: u64,
}

/// A single benchmark definition including node configuration, payload, and variable matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkDefinition {
    /// EL client type: `"base-reth-node"` or `"builder"`.
    pub node_type: String,
    /// Explicit data directory paths.
    pub datadir: DatadirConfig,
    /// Snapshot creation configuration.
    pub snapshot: Option<SnapshotConfig>,
    /// Transaction payload to submit during the benchmark.
    pub payload: TransactionPayloadDef,
    /// Prometheus threshold configuration.
    pub metrics: Option<MetricsConfig>,
    /// Chain spec name or path passed to `--chain`. When `None` a genesis file
    /// is generated automatically (devnet mode). Set to `"base"` for mainnet.
    #[serde(default)]
    pub chain: Option<String>,
    /// Extra CLI arguments for the node binary.
    #[serde(default)]
    pub node_args: Option<String>,
    /// Arbitrary key-value tags attached to the run output.
    #[serde(default)]
    pub tags: HashMap<String, String>,
    /// Matrix variables for combinatorial expansion.
    #[serde(default)]
    pub variables: Vec<Variable>,
}

/// Explicit datadir paths for sequencer and validator. When set, snapshot
/// creation is skipped and the provided path is used directly.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DatadirConfig {
    /// Explicit sequencer data directory path.
    pub sequencer: Option<PathBuf>,
    /// Explicit validator data directory path.
    pub validator: Option<PathBuf>,
}

/// Snapshot configuration for setting up a node's data directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    /// Shell command to run for snapshot creation.
    pub command: String,
    /// Optional genesis file path passed to the snapshot script.
    pub genesis_file: Option<PathBuf>,
    /// Delete existing snapshot before re-running the script.
    pub force_clean: bool,
}

/// Prometheus-based metric thresholds for warn/error alerting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// Thresholds that produce warnings.
    #[serde(default)]
    pub warning: Vec<MetricsThreshold>,
    /// Thresholds that produce errors (fail the run).
    #[serde(default)]
    pub error: Vec<MetricsThreshold>,
}

/// A single threshold bound for a named metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsThreshold {
    /// Prometheus metric name to check.
    pub metric: String,
    /// Minimum acceptable average value.
    pub min: Option<f64>,
    /// Maximum acceptable average value.
    pub max: Option<f64>,
}

/// A matrix variable with one or more values to expand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variable {
    /// Variable name substituted into tags/labels.
    pub name: String,
    /// Set of values to expand combinatorially.
    pub values: Vec<String>,
}

/// A transaction payload definition referencing a payload type and parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionPayloadDef {
    /// Unique identifier for this payload definition.
    pub id: String,
    /// Payload type discriminator (e.g. `"load-test"`).
    #[serde(rename = "type")]
    pub payload_type: String,
    /// Parameters for the load-test payload.
    pub params: LoadTestPayloadParams,
}

/// Parameters for the load-test payload type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadTestPayloadParams {
    /// Number of pre-funded sender accounts.
    pub sender_count: u64,
    /// Per-sender funding amount in wei (hex or decimal string).
    pub funding_amount: Option<String>,
    /// Weighted transaction type mix.
    #[serde(default = "default_transactions")]
    pub transactions: Vec<WeightedTx>,
}

/// A weighted transaction type entry for the load-test configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WeightedTx {
    /// Relative weight in the transaction mix.
    pub weight: u64,
    /// Transaction kind: `"transfer"`, `"calldata"`, `"erc20"`, `"precompile"`,
    /// `"uniswap_v3"`, `"aerodrome_cl"`, etc.
    #[serde(rename = "type")]
    pub tx_type: String,
    /// Maximum calldata size in bytes (`calldata` type only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
    /// Repeat count for the calldata payload (`calldata` type only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_count: Option<u64>,
    /// Target contract address or precompile name (`precompile`, `osaka`, `erc20` types).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Deployed ERC-20 contract address (`erc20` type only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    /// Iteration count for precompile benchmarking (`precompile` type only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iterations: Option<u64>,
    /// Swap router address (`uniswap_v3`, `aerodrome_cl` types).
    /// Filled in automatically when `setup.deploy_uniswap_v3` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router: Option<String>,
    /// Input token address for swaps (`uniswap_v3`, `aerodrome_cl` types).
    /// Filled in automatically when `setup.deploy_uniswap_v3` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_in: Option<String>,
    /// Output token address for swaps (`uniswap_v3`, `aerodrome_cl` types).
    /// Filled in automatically when `setup.deploy_uniswap_v3` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_out: Option<String>,
    /// Uniswap V3 fee tier in hundredths of a basis point (e.g. `3000` = 0.3%).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee: Option<u32>,
    /// Minimum swap input amount in wei (`uniswap_v3`, `aerodrome_cl` types).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_amount: Option<String>,
    /// Maximum swap input amount in wei (`uniswap_v3`, `aerodrome_cl` types).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_amount: Option<String>,
    /// Tick spacing for Aerodrome CL pools (`aerodrome_cl` type only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tick_spacing: Option<i32>,
}

fn default_transactions() -> Vec<WeightedTx> {
    vec![
        WeightedTx { weight: 70, tx_type: "transfer".into(), ..Default::default() },
        WeightedTx {
            weight: 20,
            tx_type: "calldata".into(),
            max_size: Some(256),
            ..Default::default()
        },
        WeightedTx {
            weight: 10,
            tx_type: "precompile".into(),
            target: Some("sha256".into()),
            ..Default::default()
        },
    ]
}

/// A fully expanded test run produced by matrix expansion.
#[derive(Debug, Clone)]
pub struct TestRun {
    /// Unique run identifier (random hex).
    pub id: String,
    /// Resolved variable bindings for this run.
    pub params: HashMap<String, String>,
    /// The benchmark definition this run was expanded from (includes payload and setup).
    pub definition: BenchmarkDefinition,
}

fn expand_variables(variables: &[Variable]) -> Vec<HashMap<String, String>> {
    if variables.is_empty() {
        return vec![HashMap::new()];
    }
    let mut result = vec![HashMap::new()];
    for variable in variables {
        let mut next = Vec::new();
        for existing in &result {
            for value in &variable.values {
                let mut entry = existing.clone();
                entry.insert(variable.name.clone(), value.clone());
                next.push(entry);
            }
        }
        result = next;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> BenchmarkConfig {
        BenchmarkConfig {
            name: "test".into(),
            description: None,
            block_time_ms: 1000,
            num_blocks: 10,
            parallel_tx_batches: None,
            flashblocks: None,
            benchmarks: vec![BenchmarkDefinition {
                node_type: "base-reth-node".into(),
                datadir: DatadirConfig::default(),
                snapshot: None,
                payload: TransactionPayloadDef {
                    id: "lt".into(),
                    payload_type: "load-test".into(),
                    params: LoadTestPayloadParams {
                        sender_count: 1,
                        funding_amount: None,
                        transactions: default_transactions(),
                    },
                },
                metrics: None,
                chain: None,
                node_args: None,
                tags: HashMap::new(),
                variables: vec![],
            }],
            gas_limit: None,
            rollup_config: None,
        }
    }

    #[test]
    fn expand_no_variables() {
        let config = minimal_config();
        let runs = config.expand().unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].params.is_empty());
    }

    #[test]
    fn expand_single_variable() {
        let mut config = minimal_config();
        config.benchmarks[0].variables =
            vec![Variable { name: "x".into(), values: vec!["a".into(), "b".into()] }];
        let runs = config.expand().unwrap();
        assert_eq!(runs.len(), 2);
        let vals: Vec<_> = runs.iter().map(|r| r.params["x"].as_str()).collect();
        assert!(vals.contains(&"a"));
        assert!(vals.contains(&"b"));
    }

    #[test]
    fn expand_cartesian_product() {
        let mut config = minimal_config();
        config.benchmarks[0].variables = vec![
            Variable { name: "x".into(), values: vec!["1".into(), "2".into()] },
            Variable { name: "y".into(), values: vec!["a".into(), "b".into(), "c".into()] },
        ];
        let runs = config.expand().unwrap();
        assert_eq!(runs.len(), 6);
    }

    #[test]
    fn expand_over_100_returns_error() {
        let mut config = minimal_config();
        config.benchmarks[0].variables = vec![
            Variable { name: "x".into(), values: (0..11).map(|i| i.to_string()).collect() },
            Variable { name: "y".into(), values: (0..11).map(|i| i.to_string()).collect() },
        ];
        assert!(config.expand().is_err());
    }

    #[test]
    fn config_yaml_round_trip() {
        let config = minimal_config();
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: BenchmarkConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.name, config.name);
        assert_eq!(parsed.block_time_ms, config.block_time_ms);
        assert_eq!(parsed.benchmarks.len(), 1);
    }
}
