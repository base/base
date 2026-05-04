use std::path::PathBuf;

use tracing::info;

use crate::config::BenchmarkConfig;
use crate::error::BenchmarkError;
use crate::runner::{NetworkBenchmark, RunnerOptions};

pub struct BenchmarkArgs {
    pub config_path: PathBuf,
    pub output_dir: PathBuf,
    pub reth_bin: PathBuf,
    pub builder_bin: PathBuf,
    pub load_test_bin: PathBuf,
    pub prefund_key: String,
    pub snapshot_dir: PathBuf,
}

pub async fn run_benchmark(args: BenchmarkArgs) -> Result<(), BenchmarkError> {
    let raw = std::fs::read_to_string(&args.config_path).map_err(BenchmarkError::Io)?;
    let config: BenchmarkConfig =
        serde_yaml::from_str(&raw).map_err(|e| BenchmarkError::Config(e.to_string()))?;

    std::fs::create_dir_all(&args.output_dir).map_err(BenchmarkError::Io)?;
    std::fs::create_dir_all(&args.snapshot_dir).map_err(BenchmarkError::Io)?;

    let options = RunnerOptions {
        reth_bin: args.reth_bin,
        builder_bin: args.builder_bin,
        load_test_bin: args.load_test_bin,
        output_dir: args.output_dir.clone(),
        prefund_key: args.prefund_key,
    };

    let mut runner = NetworkBenchmark::new(config, options, args.snapshot_dir);
    let results = runner.run_all().await?;

    for result in &results {
        let violation_count = result.violations.len();
        let block_count = result.block_metrics.len();
        info!(
            run_id = %result.id,
            blocks = block_count,
            violations = violation_count,
            "run finished"
        );
    }

    let error_runs: Vec<_> = results
        .iter()
        .filter(|r| {
            r.violations
                .iter()
                .any(|v| v.severity == crate::metrics::Severity::Error)
        })
        .collect();

    if !error_runs.is_empty() {
        return Err(BenchmarkError::Config(format!(
            "{} run(s) exceeded error thresholds",
            error_runs.len()
        )));
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
