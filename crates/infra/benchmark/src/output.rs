//! Output directory management, file helpers, and random ID generation.

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use base_common_rpc_types_engine::BaseExecutionPayloadV4;
use chrono::{SecondsFormat, Utc};
use flate2::{Compression, write::GzEncoder};
use rand::Rng;
use serde_json::json;
use tracing::warn;

use crate::{
    config::{BenchmarkConfig, TestRun},
    error::BenchmarkError,
    metrics::{
        BlockMetrics, GAS_PER_SECOND, GET_PAYLOAD_LATENCY, NEW_PAYLOAD_LATENCY, SEND_TXS_LATENCY,
        UPDATE_FORK_CHOICE_LATENCY,
    },
};

/// Generate a random 8-byte lowercase hex identifier.
pub fn random_id() -> String {
    let bytes: [u8; 8] = rand::rng().random();
    hex::encode(bytes)
}

/// Create the per-run output directory: `<output_dir>/<run_id>/<test_id>-<index>/`.
pub fn create_run_dir(
    output_dir: &Path,
    run_id: &str,
    test_id: &str,
    index: usize,
) -> Result<PathBuf, BenchmarkError> {
    let dir = output_dir.join(run_id).join(format!("{test_id}-{index}"));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Write a `tags.json` file to `dir`.
pub fn write_tags_json(dir: &Path, tags: &HashMap<String, String>) -> Result<(), BenchmarkError> {
    let path = dir.join("tags.json");
    let json = serde_json::to_string_pretty(tags)?;
    fs::write(path, json)?;
    Ok(())
}

/// Write a `result-<role>.json` file recording the test outcome.
pub fn write_result_json(
    dir: &Path,
    role: &str,
    test_name: &str,
    success: bool,
    error: Option<&str>,
) -> Result<(), BenchmarkError> {
    let path = dir.join(format!("result-{role}.json"));
    let value = json!({
        "test_name": test_name,
        "success": success,
        "error": error,
    });
    fs::write(path, serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

/// Gzip `src` into `dest`.
pub fn gzip_file(src: &Path, dest: &Path) -> Result<(), BenchmarkError> {
    let mut input = File::open(src)?;
    let output = File::create(dest)?;
    let mut encoder = GzEncoder::new(output, Compression::default());
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

/// Rename `metrics.json` in `src_dir` to `metrics-<role>.json` in `dest_dir`.
pub fn copy_metrics(src_dir: &Path, dest_dir: &Path, role: &str) -> Result<(), BenchmarkError> {
    let src = src_dir.join("metrics.json");
    let dest = dest_dir.join(format!("metrics-{role}.json"));
    fs::rename(src, dest)?;
    Ok(())
}

/// Write per-block metrics as `metrics-<role>.json` in `output_dir`.
pub fn write_metrics_file(
    output_dir: &Path,
    role: &str,
    block_metrics: &[BlockMetrics],
) -> Result<(), BenchmarkError> {
    let entries: Vec<serde_json::Value> = block_metrics
        .iter()
        .map(|metrics| {
            json!({
                "BlockNumber": metrics.block_number,
                "ExecutionMetrics": metrics.execution_metrics,
            })
        })
        .collect();

    let path = output_dir.join(format!("metrics-{role}.json"));
    fs::write(path, serde_json::to_string_pretty(&entries)?)?;
    Ok(())
}

/// Git/group/tag context passed alongside each metadata write.
#[derive(Debug)]
pub struct RunContext<'a> {
    /// Invocation-level group identifier.
    pub run_group_id: &'a str,
    /// Git commit SHA.
    pub git_sha: &'a str,
    /// Git branch name.
    pub git_branch: &'a str,
    /// Global user-supplied tags.
    pub global_tags: &'a HashMap<String, String>,
    /// Whether the run passed all thresholds.
    pub success: bool,
    /// Root results directory (used to compute relative `outputDir`).
    pub results_root: &'a Path,
}

/// Write a `metadata.json` summarising the run, config, and metrics.
pub fn write_metadata_json(
    output_dir: &Path,
    results_root: &Path,
    config_path: Option<&Path>,
    run: &TestRun,
    config: &BenchmarkConfig,
    sequencer_metrics: &[BlockMetrics],
    validator_metrics: &[BlockMetrics],
    ctx: &RunContext<'_>,
) -> Result<(), BenchmarkError> {
    let gas_limit = config.gas_limit.unwrap_or(30_000_000);
    let output_dir_name = output_dir
        .strip_prefix(results_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| output_dir.display().to_string());
    let source_file =
        config_path.map(|p| p.display().to_string()).unwrap_or_else(|| "<default>".to_string());

    let mut test_config = serde_json::Map::from_iter([
        ("NodeType".to_string(), json!(run.definition.node_type)),
        ("GasLimit".to_string(), json!(gas_limit)),
        ("BlockTimeMilliseconds".to_string(), json!(config.block_time_ms)),
        ("BenchmarkRun".to_string(), json!(run.id)),
        ("NodeArgs".to_string(), json!(run.definition.node_args.clone().unwrap_or_default())),
        ("ValidatorNodeType".to_string(), json!("base-reth-node")),
    ]);
    for (key, value) in &run.definition.tags {
        test_config.insert(key.clone(), json!(value));
    }

    let mut merged_tags = ctx.global_tags.clone();
    for (key, value) in &run.definition.tags {
        merged_tags.insert(key.clone(), value.clone());
    }

    let metadata = json!({
        "runs": [{
            "id": run.id,
            "runGroupId": ctx.run_group_id,
            "gitSha": ctx.git_sha,
            "gitBranch": ctx.git_branch,
            "tags": merged_tags,
            "sourceFile": source_file,
            "outputDir": output_dir_name,
            "testName": config.name,
            "testDescription": config.description.clone().unwrap_or_default(),
            "testConfig": test_config,
            "result": {
                "success": ctx.success,
                "complete": true,
                "sequencerMetrics": {
                    "gasPerSecond": average_metric(sequencer_metrics, GAS_PER_SECOND),
                    "forkChoiceUpdated": average_metric_seconds(sequencer_metrics, UPDATE_FORK_CHOICE_LATENCY),
                    "getPayload": average_metric_seconds(sequencer_metrics, GET_PAYLOAD_LATENCY),
                    "sendTxs": average_metric_seconds(sequencer_metrics, SEND_TXS_LATENCY),
                },
                "validatorMetrics": {
                    "gasPerSecond": average_metric(validator_metrics, GAS_PER_SECOND),
                    "newPayload": average_metric_seconds(validator_metrics, NEW_PAYLOAD_LATENCY),
                }
            },
            "thresholds": serde_json::Value::Null,
            "createdAt": Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
            "machineInfo": serde_json::Value::Null,
        }]
    });

    fs::write(output_dir.join("metadata.json"), serde_json::to_string_pretty(&metadata)?)?;
    Ok(())
}

/// Compute the arithmetic mean of a named metric across all blocks.
pub fn average_metric(metrics: &[BlockMetrics], metric_name: &str) -> f64 {
    let values: Vec<f64> = metrics
        .iter()
        .filter_map(|entry| entry.execution_metrics.get(metric_name).copied())
        .collect();
    if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 }
}

/// Like [`average_metric`] but converts nanoseconds to seconds.
pub fn average_metric_seconds(metrics: &[BlockMetrics], metric_name: &str) -> f64 {
    average_metric(metrics, metric_name) / 1_000_000_000.0
}

/// Serialize a list of execution payloads to `payloads.json` in `output_dir`.
pub fn write_payloads_json(
    output_dir: &Path,
    payloads: &[BaseExecutionPayloadV4],
) -> Result<(), BenchmarkError> {
    let path = output_dir.join("payloads.json");
    fs::write(path, serde_json::to_string_pretty(payloads)?)?;
    Ok(())
}

/// Load execution payloads previously written by [`write_payloads_json`].
pub fn load_payloads_json(path: &Path) -> Result<Vec<BaseExecutionPayloadV4>, BenchmarkError> {
    let raw = fs::read_to_string(path)?;
    serde_json::from_str(&raw)
        .map_err(|e| BenchmarkError::Config(format!("failed to parse payloads file: {e}")))
}

/// Print the last `max_bytes` of a log file to stderr, used on test failure.
pub fn dump_log_tail(path: &Path, max_bytes: u64) {
    let Ok(mut file) = File::open(path) else { return };
    let Ok(metadata) = file.metadata() else { return };
    let size = metadata.len();
    if size > max_bytes {
        if let Err(e) = {
            use std::io::Seek;
            file.seek(io::SeekFrom::End(-(max_bytes as i64)))
        } {
            warn!(error = %e, "failed to seek log file");
            return;
        }
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_ok() {
        let _ = io::stderr().write_all(&buf);
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn random_id_is_hex_16_chars() {
        let id = random_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn create_run_dir_makes_nested_dirs() {
        let tmp = tempdir().unwrap();
        let dir = create_run_dir(tmp.path(), "run1", "test1", 0).unwrap();
        assert!(dir.exists());
        assert!(dir.is_dir());
    }

    #[test]
    fn gzip_round_trip() {
        use flate2::read::GzDecoder;

        let tmp = tempdir().unwrap();
        let src = tmp.path().join("input.txt");
        let dest = tmp.path().join("output.gz");
        fs::write(&src, b"hello benchmark").unwrap();
        gzip_file(&src, &dest).unwrap();
        assert!(dest.exists());

        let f = File::open(&dest).unwrap();
        let mut decoder = GzDecoder::new(f);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello benchmark");
    }
}
