//! Block metrics collection, Prometheus scraping, and threshold checking.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use tracing::warn;

use crate::error::BenchmarkError;

pub const SEND_TXS_LATENCY: &str = "latency/send_txs";
pub const UPDATE_FORK_CHOICE_LATENCY: &str = "latency/fork_choice_updated";
pub const GET_PAYLOAD_LATENCY: &str = "latency/get_payload";
pub const NEW_PAYLOAD_LATENCY: &str = "latency/new_payload";
pub const GAS_PER_BLOCK: &str = "gas/per_block";
pub const GAS_PER_SECOND: &str = "gas/per_second";
pub const TRANSACTIONS_PER_BLOCK: &str = "transactions/per_block";

/// Per-block metrics collected during a benchmark run.
#[derive(Debug, Clone)]
pub struct BlockMetrics {
    /// Block number this entry corresponds to.
    pub block_number: u64,
    /// Wall-clock time when the block was processed.
    pub timestamp: Instant,
    /// Scraped Prometheus samples from the previous block (for delta calculation).
    pub prev_prometheus: HashMap<String, prometheus_parse::Sample>,
    /// Named execution metrics (latencies, gas, tx counts).
    pub execution_metrics: HashMap<String, f64>,
}

impl BlockMetrics {
    /// Create a new entry for the given block number.
    pub fn new(block_number: u64) -> Self {
        Self {
            block_number,
            timestamp: Instant::now(),
            prev_prometheus: HashMap::new(),
            execution_metrics: HashMap::new(),
        }
    }

    /// Record a named execution metric value.
    pub fn add_execution_metric(&mut self, name: &str, value: f64) {
        self.execution_metrics.insert(name.to_string(), value);
    }

    /// For every `<base>_sum` / `<base>_count` Counter pair, computes
    /// `delta_sum / delta_count` relative to the previous scrape and inserts
    /// the result as `<base>_avg`. Matches the Go runner's per-interval histogram logic.
    pub fn compute_delta_averages(&mut self) {
        use prometheus_parse::Value;

        let sum_keys: Vec<String> = self
            .execution_metrics
            .keys()
            .filter(|k| k.ends_with("_sum"))
            .cloned()
            .collect();

        for sum_key in sum_keys {
            let base = &sum_key[..sum_key.len() - 4];
            let count_key = format!("{base}_count");

            let cur_sum = match self.execution_metrics.get(&sum_key).copied() {
                Some(v) => v,
                None => continue,
            };
            let cur_count = match self.execution_metrics.get(&count_key).copied() {
                Some(v) => v,
                None => continue,
            };

            let prev_sum = self
                .prev_prometheus
                .get(&sum_key)
                .and_then(|s| if let Value::Counter(v) | Value::Gauge(v) | Value::Untyped(v) = s.value { Some(v) } else { None })
                .unwrap_or(0.0);
            let prev_count = self
                .prev_prometheus
                .get(&count_key)
                .and_then(|s| if let Value::Counter(v) | Value::Gauge(v) | Value::Untyped(v) = s.value { Some(v) } else { None })
                .unwrap_or(0.0);

            let delta_count = cur_count - prev_count;
            if delta_count > 0.0 {
                let avg = (cur_sum - prev_sum) / delta_count;
                if !avg.is_nan() {
                    self.execution_metrics.insert(format!("{base}_avg"), avg);
                }
            }
        }
    }

    /// Update a metric from a new Prometheus sample, computing deltas for
    /// Histogram and Summary types (deltaSum / deltaCount).
    ///
    /// NaN results (e.g. from zero delta count) are silently skipped.
    pub fn update_prometheus_metric(
        &mut self,
        name: &str,
        current: &prometheus_parse::Sample,
    ) {
        use prometheus_parse::Value;

        let value = match &current.value {
            Value::Histogram(buckets) => {
                let (cur_sum, cur_count) = histogram_sum_count(buckets);
                if let Some(prev) = self.prev_prometheus.get(name) {
                    if let Value::Histogram(prev_buckets) = &prev.value {
                        let (prev_sum, prev_count) = histogram_sum_count(prev_buckets);
                        let delta_count = cur_count - prev_count;
                        if delta_count == 0.0 {
                            None
                        } else {
                            Some((cur_sum - prev_sum) / delta_count)
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            Value::Summary(quantiles) => {
                let (cur_sum, cur_count) = summary_sum_count(quantiles);
                if let Some(prev) = self.prev_prometheus.get(name) {
                    if let Value::Summary(prev_quantiles) = &prev.value {
                        let (prev_sum, prev_count) = summary_sum_count(prev_quantiles);
                        let delta_count = cur_count - prev_count;
                        if delta_count == 0.0 {
                            None
                        } else {
                            Some((cur_sum - prev_sum) / delta_count)
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            Value::Gauge(v) | Value::Counter(v) | Value::Untyped(v) => Some(*v),
        };

        if let Some(v) = value {
            if !v.is_nan() {
                self.execution_metrics.insert(name.to_string(), v);
            }
        }
        self.prev_prometheus.insert(name.to_string(), current.clone());
    }
}

fn histogram_sum_count(buckets: &[prometheus_parse::HistogramCount]) -> (f64, f64) {
    let sum = buckets.iter().filter(|b| b.less_than == f64::INFINITY).map(|b| b.count).sum();
    let count = buckets.last().map(|b| b.count).unwrap_or(0.0);
    (sum, count)
}

fn summary_sum_count(quantiles: &[prometheus_parse::SummaryCount]) -> (f64, f64) {
    let sum = quantiles.iter().map(|q| q.count).sum();
    let count = quantiles.len() as f64;
    (sum, count)
}

/// Scrape Prometheus metrics from a node's metrics endpoint.
pub async fn scrape_prometheus(
    url: &str,
) -> Result<Vec<prometheus_parse::Sample>, BenchmarkError> {
    let text = reqwest::get(url)
        .await
        .map_err(|e| BenchmarkError::Metrics(format!("scrape failed: {e}")))?
        .text()
        .await
        .map_err(|e| BenchmarkError::Metrics(format!("scrape body error: {e}")))?;

    let lines: Vec<&str> = text.lines().collect();
    let scrape = prometheus_parse::Scrape::parse(lines.into_iter().map(|l| Ok(l.to_owned())))
        .map_err(|e| BenchmarkError::Metrics(format!("prometheus parse error: {e}")))?;

    Ok(scrape.samples)
}

/// Collects and stores per-block metrics for a benchmark run.
pub struct MetricsCollector {
    metrics_url: String,
    collected: Vec<BlockMetrics>,
}

impl MetricsCollector {
    /// Create a collector that scrapes `http://127.0.0.1:<port>/metrics`.
    pub fn new(metrics_port: u16) -> Self {
        Self {
            metrics_url: format!("http://127.0.0.1:{metrics_port}/metrics"),
            collected: Vec::new(),
        }
    }

    /// Scrape Prometheus and update the given block metrics entry.
    pub async fn collect(&mut self, block: &mut BlockMetrics) -> Result<(), BenchmarkError> {
        let samples = match scrape_prometheus(&self.metrics_url).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, url = %self.metrics_url, "prometheus scrape failed, skipping");
                return Ok(());
            }
        };
        for sample in &samples {
            block.update_prometheus_metric(&sample.metric, sample);
        }
        block.compute_delta_averages();
        Ok(())
    }

    /// Return all collected block metrics.
    pub fn get_metrics(&self) -> &[BlockMetrics] {
        &self.collected
    }

    /// Store a completed block metrics entry.
    pub fn push(&mut self, metrics: BlockMetrics) {
        self.collected.push(metrics);
    }
}

/// Severity of a metrics threshold violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    /// Metric exceeded warning threshold.
    Warning,
    /// Metric exceeded error threshold.
    Error,
}

/// A single threshold violation found after a benchmark run.
#[derive(Debug, Clone)]
pub struct ThresholdViolation {
    /// Name of the metric that violated its threshold.
    pub metric: String,
    /// Observed value.
    pub value: f64,
    /// Threshold bound that was exceeded.
    pub bound: f64,
    /// Severity level.
    pub severity: Severity,
}

/// Write all block metrics as a JSON array to `path`.
pub fn write_metrics_json(metrics: &[BlockMetrics], path: &Path) -> Result<(), BenchmarkError> {
    let serializable: Vec<serde_json::Value> = metrics
        .iter()
        .map(|m| {
            serde_json::json!({
                "block_number": m.block_number,
                "execution_metrics": m.execution_metrics,
            })
        })
        .collect();
    let json = serde_json::to_string_pretty(&serializable)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Check collected metrics against configured thresholds.
pub fn check_thresholds(
    metrics: &[BlockMetrics],
    config: &crate::config::MetricsConfig,
) -> Vec<ThresholdViolation> {
    let mut violations = Vec::new();

    let check = |thresholds: &[crate::config::MetricsThreshold],
                 severity: Severity,
                 violations: &mut Vec<ThresholdViolation>| {
        for threshold in thresholds {
            let values: Vec<f64> = metrics
                .iter()
                .filter_map(|m| m.execution_metrics.get(&threshold.metric).copied())
                .collect();
            if values.is_empty() {
                continue;
            }
            let avg = values.iter().sum::<f64>() / values.len() as f64;
            if let Some(min) = threshold.min {
                if avg < min {
                    warn!(
                        metric = %threshold.metric,
                        value = %avg,
                        bound = %min,
                        "metric below minimum threshold",
                    );
                    violations.push(ThresholdViolation {
                        metric: threshold.metric.clone(),
                        value: avg,
                        bound: min,
                        severity: severity.clone(),
                    });
                }
            }
            if let Some(max) = threshold.max {
                if avg > max {
                    warn!(
                        metric = %threshold.metric,
                        value = %avg,
                        bound = %max,
                        "metric above maximum threshold",
                    );
                    violations.push(ThresholdViolation {
                        metric: threshold.metric.clone(),
                        value: avg,
                        bound: max,
                        severity: severity.clone(),
                    });
                }
            }
        }
    };

    check(&config.warning, Severity::Warning, &mut violations);
    check(&config.error, Severity::Error, &mut violations);
    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MetricsConfig, MetricsThreshold};

    #[test]
    fn block_metrics_add_execution_metric() {
        let mut m = BlockMetrics::new(42);
        m.add_execution_metric(GAS_PER_BLOCK, 1_000_000.0);
        assert_eq!(m.execution_metrics[GAS_PER_BLOCK], 1_000_000.0);
    }

    #[test]
    fn block_metrics_gauge_raw_value() {
        let text = "# TYPE reth_gas gauge\nreth_gas 42.0\n";
        let samples =
            prometheus_parse::Scrape::parse(text.lines().map(|l| Ok(l.to_owned()))).unwrap();
        let mut m = BlockMetrics::new(1);
        for s in &samples.samples {
            m.update_prometheus_metric("reth_gas", s);
        }
        assert_eq!(m.execution_metrics["reth_gas"], 42.0);
    }

    #[test]
    fn threshold_check_min_violation() {
        let mut m = BlockMetrics::new(1);
        m.add_execution_metric(GAS_PER_BLOCK, 500.0);
        let config = MetricsConfig {
            warning: vec![MetricsThreshold {
                metric: GAS_PER_BLOCK.into(),
                min: Some(1000.0),
                max: None,
            }],
            error: vec![],
        };
        let violations = check_thresholds(&[m], &config);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Warning);
    }

    #[test]
    fn threshold_check_max_violation() {
        let mut m = BlockMetrics::new(1);
        m.add_execution_metric(GAS_PER_BLOCK, 5_000_000.0);
        let config = MetricsConfig {
            warning: vec![],
            error: vec![MetricsThreshold {
                metric: GAS_PER_BLOCK.into(),
                min: None,
                max: Some(1_000_000.0),
            }],
        };
        let violations = check_thresholds(&[m], &config);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Error);
    }

    #[test]
    fn threshold_check_no_violation() {
        let mut m = BlockMetrics::new(1);
        m.add_execution_metric(GAS_PER_BLOCK, 500_000.0);
        let config = MetricsConfig {
            warning: vec![MetricsThreshold {
                metric: GAS_PER_BLOCK.into(),
                min: Some(100_000.0),
                max: Some(1_000_000.0),
            }],
            error: vec![],
        };
        let violations = check_thresholds(&[m], &config);
        assert!(violations.is_empty());
    }
}
