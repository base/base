//! Top-level entry point: parse config, create runner, report results.

use std::path::PathBuf;

use tracing::info;

use crate::config::BenchmarkConfig;
use crate::error::BenchmarkError;
use crate::runner::{NetworkBenchmark, RunnerOptions};

/// The embedded default benchmark config (ERC-20 transfers on a fresh devnet).
pub const DEFAULT_CONFIG_YAML: &str = include_str!("../examples/devnet.yaml");

/// Parse a YAML string into a [`BenchmarkConfig`].
pub fn parse_config(yaml: &str) -> Result<BenchmarkConfig, BenchmarkError> {
    serde_yaml::from_str(yaml).map_err(|e| BenchmarkError::Config(e.to_string()))
}

/// CLI-resolved arguments for [`run_benchmark`].
#[derive(Debug)]
pub struct BenchmarkArgs {
    /// Parsed benchmark configuration.
    pub config: BenchmarkConfig,
    /// Source path of the config file, if one was provided (recorded in metadata).
    pub config_path: Option<PathBuf>,
    /// Directory for writing results.
    pub output_dir: PathBuf,
    /// Path to the `base-reth-node` binary.
    pub reth_bin: PathBuf,
    /// Path to the `base-builder` binary.
    pub builder_bin: PathBuf,
    /// Path to the `base-load-test` binary.
    pub load_test_bin: PathBuf,
    /// Hex-encoded private key for pre-funding.
    pub prefund_key: String,
    /// Directory for cached snapshots.
    pub snapshot_dir: PathBuf,
}

/// Run all benchmark entries from the pre-parsed config and log results.
pub async fn run_benchmark(args: BenchmarkArgs) -> Result<(), BenchmarkError> {
    std::fs::create_dir_all(&args.output_dir).map_err(BenchmarkError::Io)?;
    std::fs::create_dir_all(&args.snapshot_dir).map_err(BenchmarkError::Io)?;

    let options = RunnerOptions {
        reth_bin: args.reth_bin,
        builder_bin: args.builder_bin,
        load_test_bin: args.load_test_bin,
        config_path: args.config_path,
        output_dir: args.output_dir.clone(),
        prefund_key: args.prefund_key,
    };

    let mut runner = NetworkBenchmark::new(args.config, options, args.snapshot_dir);
    let results = runner.run_all().await?;

    for result in &results {
        let violation_count = result.violations.len();
        let block_count = result.block_metrics.len();
        let validator_block_count = result.validator_block_metrics.len();
        info!(
            run_id = %result.id,
            sequencer_blocks = block_count,
            validator_blocks = validator_block_count,
            violations = violation_count,
            "run finished"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchmark_args_fields_accessible() {
        let config = parse_config(DEFAULT_CONFIG_YAML).expect("default config should parse");
        let args = BenchmarkArgs {
            config,
            config_path: Some(PathBuf::from("/tmp/config.yaml")),
            output_dir: PathBuf::from("/tmp/out"),
            reth_bin: PathBuf::from("/bin/reth"),
            builder_bin: PathBuf::from("/bin/builder"),
            load_test_bin: PathBuf::from("/bin/load-test"),
            prefund_key: "0xdeadbeef".into(),
            snapshot_dir: PathBuf::from("/tmp/snapshots"),
        };
        assert_eq!(args.config_path, Some(PathBuf::from("/tmp/config.yaml")));
    }

    #[test]
    fn default_config_yaml_parses() {
        parse_config(DEFAULT_CONFIG_YAML).expect("default config should parse");
    }
}
