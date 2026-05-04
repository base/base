//! Benchmark configuration types and matrix expansion.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::BenchmarkError;

/// Top-level benchmark configuration loaded from YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    pub name: String,
    pub description: Option<String>,
    pub block_time_ms: u64,
    pub num_blocks: u64,
    pub gas_limit: Option<u64>,
    pub rollup_config: Option<PathBuf>,
    pub parallel_tx_batches: Option<u64>,
    pub flashblocks: Option<FlashblocksConfig>,
    pub transaction_payloads: Vec<TransactionPayloadDef>,
    pub benchmarks: Vec<BenchmarkDefinition>,
}

impl BenchmarkConfig {
    /// Expand all benchmark definitions into a flat list of [`TestRun`]s via
    /// cartesian product of each definition's variables. Returns an error if
    /// the expansion would exceed 100 runs.
    pub fn expand(&self) -> Result<Vec<TestRun>, BenchmarkError> {
        let mut runs = Vec::new();
        for definition in &self.benchmarks {
            let payload = self
                .transaction_payloads
                .first()
                .ok_or_else(|| {
                    BenchmarkError::Config("at least one transaction_payload required".into())
                })?
                .clone();
            let expanded = expand_variables(&definition.variables);
            for params in expanded {
                runs.push(TestRun {
                    id: crate::output::random_id(),
                    params,
                    definition: definition.clone(),
                    payload: payload.clone(),
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
    pub block_time_ms: u64,
}

/// A single benchmark definition including node configuration and variable matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkDefinition {
    pub node_type: String,
    pub datadir: DatadirConfig,
    pub snapshot: Option<SnapshotConfig>,
    pub metrics: Option<MetricsConfig>,
    #[serde(default)]
    pub node_args: Option<String>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    #[serde(default)]
    pub variables: Vec<Variable>,
}

/// Explicit datadir paths for sequencer and validator. When set, snapshot
/// creation is skipped and the provided path is used directly.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DatadirConfig {
    pub sequencer: Option<PathBuf>,
    pub validator: Option<PathBuf>,
}

/// Snapshot configuration for setting up a node's data directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    pub command: String,
    pub genesis_file: Option<PathBuf>,
    pub force_clean: bool,
}

/// Prometheus-based metric thresholds for warn/error alerting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    #[serde(default)]
    pub warning: Vec<MetricsThreshold>,
    #[serde(default)]
    pub error: Vec<MetricsThreshold>,
}

/// A single threshold bound for a named metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsThreshold {
    pub metric: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// A matrix variable with one or more values to expand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub values: Vec<String>,
}

/// A transaction payload definition referencing a payload type and parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionPayloadDef {
    pub id: String,
    #[serde(rename = "type")]
    pub payload_type: String,
    pub params: LoadTestPayloadParams,
}

/// Parameters for the load-test payload type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadTestPayloadParams {
    pub sender_count: u64,
    pub funding_amount: Option<String>,
    #[serde(default = "default_transactions")]
    pub transactions: Vec<WeightedTx>,
}

/// A weighted transaction type entry for the load-test configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightedTx {
    pub weight: u64,
    #[serde(rename = "type")]
    pub tx_type: String,
    pub max_size: Option<u64>,
    pub target: Option<String>,
}

fn default_transactions() -> Vec<WeightedTx> {
    vec![
        WeightedTx { weight: 70, tx_type: "transfer".into(), max_size: None, target: None },
        WeightedTx {
            weight: 20,
            tx_type: "calldata".into(),
            max_size: Some(256),
            target: None,
        },
        WeightedTx {
            weight: 10,
            tx_type: "precompile".into(),
            max_size: None,
            target: Some("sha256".into()),
        },
    ]
}

/// A fully expanded test run produced by matrix expansion.
#[derive(Debug, Clone)]
pub struct TestRun {
    pub id: String,
    pub params: HashMap<String, String>,
    pub definition: BenchmarkDefinition,
    pub payload: TransactionPayloadDef,
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
            transaction_payloads: vec![TransactionPayloadDef {
                id: "lt".into(),
                payload_type: "load-test".into(),
                params: LoadTestPayloadParams {
                    sender_count: 1,
                    funding_amount: None,
                    transactions: default_transactions(),
                },
            }],
            benchmarks: vec![BenchmarkDefinition {
                node_type: "base-reth-node".into(),
                datadir: DatadirConfig::default(),
                snapshot: None,
                metrics: None,
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
            Variable {
                name: "x".into(),
                values: (0..11).map(|i| i.to_string()).collect(),
            },
            Variable {
                name: "y".into(),
                values: (0..11).map(|i| i.to_string()).collect(),
            },
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
        assert_eq!(parsed.transaction_payloads.len(), 1);
    }
}
