//! Comparison of two benchmark run groups from a results.jsonl index.

use std::io::BufRead;
use std::path::Path;

use tracing::warn;

use crate::error::BenchmarkError;
use crate::output::ResultsIndexEntry;

/// Outcome of a benchmark comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOutcome {
    /// Challenger is meaningfully better.
    Better,
    /// Challenger is meaningfully worse.
    Worse,
    /// No significant difference or insufficient data.
    Neutral,
}

/// Summary statistics for one run group.
#[derive(Debug, Clone)]
pub struct GroupStats {
    /// Number of runs in the group.
    pub run_count: usize,
    /// Mean sequencer gas per second.
    pub avg_gas_per_second: f64,
    /// Mean `get_payload` latency (seconds).
    pub avg_get_payload_ms: f64,
    /// Mean `new_payload` latency (seconds).
    pub avg_new_payload_ms: f64,
}

/// Result of comparing two run groups.
#[derive(Debug)]
pub struct CompareResult {
    /// Statistics for the baseline group.
    pub baseline: GroupStats,
    /// Statistics for the challenger group.
    pub challenger: GroupStats,
    /// Overall outcome of the comparison.
    pub outcome: CompareOutcome,
    /// Human-readable summary lines (printed by caller).
    pub summary: Vec<String>,
}

/// Minimum percentage delta (absolute) to classify as `Better` or `Worse`.
const SIGNIFICANCE_THRESHOLD: f64 = 2.0;

/// Compare two benchmark run groups read from a `results.jsonl` index file.
///
/// Returns a [`CompareResult`] with per-group statistics, an outcome classification,
/// and pre-formatted summary lines suitable for display.
///
/// # Exit-code convention (for callers)
///
/// | [`CompareOutcome`] | Suggested exit code |
/// |--------------------|---------------------|
/// | `Better`           | 0                   |
/// | `Worse`            | 1                   |
/// | `Neutral`          | 2                   |
pub fn compare_run_groups(
    results_file: &Path,
    baseline_id: &str,
    challenger_id: &str,
) -> Result<CompareResult, BenchmarkError> {
    let file = std::fs::File::open(results_file)?;
    let reader = std::io::BufReader::new(file);

    let mut baseline_entries: Vec<ResultsIndexEntry> = Vec::new();
    let mut challenger_entries: Vec<ResultsIndexEntry> = Vec::new();

    for (line_number, line_result) in reader.lines().enumerate() {
        let line = line_result?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let entry: ResultsIndexEntry = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(e) => {
                warn!(line = line_number + 1, error = %e, "skipping malformed results line");
                continue;
            }
        };

        if entry.run_group_id == baseline_id {
            baseline_entries.push(entry);
        } else if entry.run_group_id == challenger_id {
            challenger_entries.push(entry);
        }
    }

    if baseline_entries.is_empty() {
        return Err(BenchmarkError::Config(format!(
            "no runs found for baseline group '{baseline_id}'"
        )));
    }
    if challenger_entries.is_empty() {
        return Err(BenchmarkError::Config(format!(
            "no runs found for challenger group '{challenger_id}'"
        )));
    }

    let baseline = compute_group_stats(&baseline_entries);
    let challenger = compute_group_stats(&challenger_entries);

    let gas_delta_pct =
        (challenger.avg_gas_per_second - baseline.avg_gas_per_second)
            / baseline.avg_gas_per_second
            * 100.0;

    let baseline_latency = baseline.avg_get_payload_ms + baseline.avg_new_payload_ms;
    let challenger_latency = challenger.avg_get_payload_ms + challenger.avg_new_payload_ms;
    let latency_delta_pct =
        (challenger_latency - baseline_latency) / baseline_latency * 100.0;

    let outcome = if gas_delta_pct >= SIGNIFICANCE_THRESHOLD
        || latency_delta_pct <= -SIGNIFICANCE_THRESHOLD
    {
        CompareOutcome::Better
    } else if gas_delta_pct <= -SIGNIFICANCE_THRESHOLD
        || latency_delta_pct >= SIGNIFICANCE_THRESHOLD
    {
        CompareOutcome::Worse
    } else {
        CompareOutcome::Neutral
    };

    let get_payload_delta_pct = (challenger.avg_get_payload_ms - baseline.avg_get_payload_ms)
        / baseline.avg_get_payload_ms
        * 100.0;
    let new_payload_delta_pct = (challenger.avg_new_payload_ms - baseline.avg_new_payload_ms)
        / baseline.avg_new_payload_ms
        * 100.0;

    let outcome_label = match outcome {
        CompareOutcome::Better => "BETTER",
        CompareOutcome::Worse => "WORSE",
        CompareOutcome::Neutral => "NEUTRAL",
    };

    // Display latencies as milliseconds (stored values are in seconds).
    let summary = vec![
        format!("Baseline:   {baseline_id} ({} runs)", baseline.run_count),
        format!("Challenger: {challenger_id} ({} runs)", challenger.run_count),
        String::new(),
        "Metric               Baseline      Challenger    Delta".to_string(),
        "-------------------  -----------   -----------   ------".to_string(),
        format!(
            "Gas/s (sequencer)    {:<14.0}{:<14.0}{:+.1}%",
            baseline.avg_gas_per_second, challenger.avg_gas_per_second, gas_delta_pct,
        ),
        format!(
            "getPayload (ms)     {:<14.3}{:<14.3}{:+.1}%",
            baseline.avg_get_payload_ms * 1000.0,
            challenger.avg_get_payload_ms * 1000.0,
            get_payload_delta_pct,
        ),
        format!(
            "newPayload (ms)     {:<14.3}{:<14.3}{:+.1}%",
            baseline.avg_new_payload_ms * 1000.0,
            challenger.avg_new_payload_ms * 1000.0,
            new_payload_delta_pct,
        ),
        String::new(),
        format!("Outcome: {outcome_label}"),
    ];

    Ok(CompareResult {
        baseline,
        challenger,
        outcome,
        summary,
    })
}

/// Compute mean statistics for a non-empty slice of result entries.
fn compute_group_stats(entries: &[ResultsIndexEntry]) -> GroupStats {
    let n = entries.len() as f64;
    let avg_gas_per_second = entries.iter().map(|e| e.gas_per_second_sequencer).sum::<f64>() / n;
    let avg_get_payload_ms = entries.iter().map(|e| e.get_payload_ms).sum::<f64>() / n;
    let avg_new_payload_ms = entries.iter().map(|e| e.new_payload_ms).sum::<f64>() / n;

    GroupStats {
        run_count: entries.len(),
        avg_gas_per_second,
        avg_get_payload_ms,
        avg_new_payload_ms,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn make_entry(group_id: &str, gas: f64, get_payload: f64, new_payload: f64) -> String {
        let entry = ResultsIndexEntry {
            run_id: format!("run-{gas}"),
            run_group_id: group_id.to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            git_sha: "abc123".to_string(),
            git_branch: "main".to_string(),
            config_name: "devnet".to_string(),
            node_type: "base-reth-node".to_string(),
            output_dir: "/tmp/out".to_string(),
            success: true,
            tags: HashMap::new(),
            gas_per_second_sequencer: gas,
            get_payload_ms: get_payload,
            new_payload_ms: new_payload,
        };
        serde_json::to_string(&entry).unwrap()
    }

    #[test]
    fn compare_better_and_worse() {
        let mut file = NamedTempFile::new().unwrap();

        // Baseline: 1_000_000 gas/s
        writeln!(file, "{}", make_entry("baseline", 1_000_000.0, 0.005, 0.003)).unwrap();
        writeln!(file, "{}", make_entry("baseline", 1_000_000.0, 0.005, 0.003)).unwrap();

        // Challenger: 20% more gas/s → clearly better
        writeln!(file, "{}", make_entry("challenger", 1_200_000.0, 0.005, 0.003)).unwrap();
        writeln!(file, "{}", make_entry("challenger", 1_200_000.0, 0.005, 0.003)).unwrap();

        let result = compare_run_groups(file.path(), "baseline", "challenger").unwrap();
        assert_eq!(result.outcome, CompareOutcome::Better);
        assert_eq!(result.baseline.run_count, 2);
        assert_eq!(result.challenger.run_count, 2);

        // Now test worse: swap roles
        let result = compare_run_groups(file.path(), "challenger", "baseline").unwrap();
        assert_eq!(result.outcome, CompareOutcome::Worse);
    }

    #[test]
    fn compare_neutral_when_delta_small() {
        let mut file = NamedTempFile::new().unwrap();

        // 1% difference — below 2% threshold
        writeln!(file, "{}", make_entry("baseline", 1_000_000.0, 0.005, 0.003)).unwrap();
        writeln!(file, "{}", make_entry("challenger", 1_010_000.0, 0.00495, 0.00297)).unwrap();

        let result = compare_run_groups(file.path(), "baseline", "challenger").unwrap();
        assert_eq!(result.outcome, CompareOutcome::Neutral);
    }

    #[test]
    fn compare_missing_group_returns_error() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "{}", make_entry("baseline", 1_000_000.0, 0.005, 0.003)).unwrap();

        let result = compare_run_groups(file.path(), "baseline", "nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nonexistent"), "error should mention missing group: {err}");
    }

    #[test]
    fn compare_missing_file_returns_error() {
        let result = compare_run_groups(Path::new("/tmp/nonexistent-results.jsonl"), "a", "b");
        assert!(result.is_err());
    }
}
