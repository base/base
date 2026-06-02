//! Top-level entry point: parse config, create runner, report results.

use std::path::PathBuf;

use tracing::info;

use crate::config::BenchmarkConfig;
use crate::error::BenchmarkError;
use crate::runner::{NetworkBenchmark, RunnerOptions};

/// CLI-resolved arguments for [`run_benchmark`].
#[derive(Debug)]
pub struct BenchmarkArgs {
    /// Path to the benchmark YAML config file.
    pub config_path: PathBuf,
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

/// Run all benchmark entries from the config file and log results.
pub async fn run_benchmark(args: BenchmarkArgs) -> Result<(), BenchmarkError> {
    let raw = std::fs::read_to_string(&args.config_path).map_err(BenchmarkError::Io)?;
    let config: BenchmarkConfig =
        serde_yaml::from_str(&raw).map_err(|e| BenchmarkError::Config(e.to_string()))?;
    let config_path = std::fs::canonicalize(&args.config_path).map_err(BenchmarkError::Io)?;

    std::fs::create_dir_all(&args.output_dir).map_err(BenchmarkError::Io)?;
    std::fs::create_dir_all(&args.snapshot_dir).map_err(BenchmarkError::Io)?;

    let options = RunnerOptions {
        reth_bin: args.reth_bin,
        builder_bin: args.builder_bin,
        load_test_bin: args.load_test_bin,
        config_path,
        output_dir: args.output_dir.clone(),
        prefund_key: args.prefund_key,
    };

    let mut runner = NetworkBenchmark::new(config, options, args.snapshot_dir);
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
        let args = BenchmarkArgs {
            config_path: PathBuf::from("/tmp/config.yaml"),
            output_dir: PathBuf::from("/tmp/out"),
            reth_bin: PathBuf::from("/bin/reth"),
            builder_bin: PathBuf::from("/bin/builder"),
            load_test_bin: PathBuf::from("/bin/load-test"),
            prefund_key: "0xdeadbeef".into(),
            snapshot_dir: PathBuf::from("/tmp/snapshots"),
        };
        assert_eq!(args.config_path, PathBuf::from("/tmp/config.yaml"));
    }
}
