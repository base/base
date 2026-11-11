//! Compares a benchmark run's transaction profile with mainnet targets.

use std::collections::BTreeMap;

use alloy_consensus::{Transaction as _, TxReceipt as _};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_network::Base;
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

const HEAVY_GAS: u64 = 400_000;
/// Fjord receipts report a transaction's estimated compressed size times 16 as L1 gas used.
const L1_GAS_PER_DA_BYTE: f64 = 16.0;

/// Receipt and calldata figures for one non-deposit transaction.
#[derive(Debug, Clone, Copy)]
pub struct TxProfile {
    gas_used: u64,
    calldata_bytes: u64,
    logs: u64,
    l1_gas_used: u128,
}

impl TxProfile {
    /// Computes the metrics `etc/scripts/local/mainnet-mix.py` emits as targets.
    fn metrics(txs: &[Self]) -> BTreeMap<&'static str, f64> {
        let count = txs.len() as f64;
        let gas = txs.iter().map(|tx| tx.gas_used as f64).sum::<f64>();
        if gas == 0.0 {
            return BTreeMap::new();
        }
        let heavy = txs.iter().filter(|tx| tx.gas_used >= HEAVY_GAS);
        let sum = |field: fn(&Self) -> f64| txs.iter().map(field).sum::<f64>();
        [
            ("gas_per_tx", gas / count),
            ("heavy_tx_share", heavy.clone().count() as f64 / count),
            ("heavy_gas_share", heavy.map(|tx| tx.gas_used as f64).sum::<f64>() / gas),
            ("calldata_bytes_per_tx", sum(|tx| tx.calldata_bytes as f64) / count),
            ("logs_per_tx", sum(|tx| tx.logs as f64) / count),
            ("da_bytes_per_gas", sum(|tx| tx.l1_gas_used as f64) / L1_GAS_PER_DA_BYTE / gas),
        ]
        .into()
    }

    /// Profiles every non-deposit transaction in blocks `start+1..=end`, the measured window.
    pub async fn measure(rpc_url: &url::Url, start: u64, end: u64) -> Result<Vec<Self>> {
        let provider = RootProvider::<Base>::new_http(rpc_url.clone());
        let mut txs = Vec::new();
        for number in start.saturating_add(1)..=end {
            let block = provider
                .get_block_by_number(BlockNumberOrTag::Number(number))
                .full()
                .await?
                .ok_or_else(|| eyre::eyre!("block {number} is unavailable from {rpc_url}"))?;
            let receipts = provider
                .get_block_receipts(number.into())
                .await?
                .ok_or_else(|| eyre::eyre!("receipts for block {number} are unavailable"))?;
            let block_txs = block.transactions.as_transactions().unwrap_or_default();
            eyre::ensure!(
                block_txs.len() == receipts.len(),
                "block {number} has {} transactions but {} receipts",
                block_txs.len(),
                receipts.len()
            );
            for (tx, receipt) in block_txs.iter().zip(&receipts) {
                if tx.as_ref().is_deposit() {
                    continue;
                }
                txs.push(Self {
                    gas_used: receipt.inner.gas_used,
                    calldata_bytes: tx.input().len() as u64,
                    logs: receipt.inner.inner.logs().len() as u64,
                    l1_gas_used: receipt.l1_block_info.l1_gas_used.unwrap_or_default(),
                });
            }
        }
        Ok(txs)
    }
}

#[derive(Debug, Deserialize)]
struct Targets {
    metrics: BTreeMap<String, Target>,
}

#[derive(Debug, Deserialize)]
struct Target {
    target: Option<f64>,
    tolerance: f64,
}

#[derive(Debug, Serialize, PartialEq)]
struct MetricCalibration {
    target: Option<f64>,
    measured: Option<f64>,
    /// `measured / target - 1`.
    error: Option<f64>,
    tolerance: f64,
    within_tolerance: Option<bool>,
}

/// A run's per-metric error against a mix's targets file.
#[derive(Debug, Serialize)]
pub struct Calibration {
    transactions: usize,
    /// Whether every metric with a target is within tolerance.
    within_tolerance: bool,
    metrics: BTreeMap<String, MetricCalibration>,
}

impl Calibration {
    /// Compares measured transactions with a targets JSON document.
    pub fn compare(targets_json: &str, txs: &[TxProfile]) -> Result<Self> {
        let targets: Targets =
            serde_json::from_str(targets_json).wrap_err("invalid calibration targets")?;
        let measured = TxProfile::metrics(txs);
        let metrics: BTreeMap<_, _> = targets
            .metrics
            .into_iter()
            .map(|(name, Target { target, tolerance })| {
                let measured = measured.get(name.as_str()).copied();
                let error = target.zip(measured).map(|(target, measured)| measured / target - 1.0);
                let within_tolerance = error.map(|error| error.abs() <= tolerance);
                (name, MetricCalibration { target, measured, error, tolerance, within_tolerance })
            })
            .collect();
        let within_tolerance = metrics
            .values()
            .all(|metric| metric.target.is_none() || metric.within_tolerance == Some(true));
        Ok(Self { transactions: txs.len(), within_tolerance, metrics })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(gas_used: u64, calldata_bytes: u64, logs: u64, l1_gas_used: u128) -> TxProfile {
        TxProfile { gas_used, calldata_bytes, logs, l1_gas_used }
    }

    #[test]
    fn metrics_cover_mean_tail_calldata_logs_and_da() {
        let txs =
            [tx(100_000, 100, 1, 1_600), tx(400_000, 300, 5, 3_200), tx(100_000, 200, 3, 1_600)];
        let metrics = TxProfile::metrics(&txs);
        assert_eq!(metrics["gas_per_tx"], 200_000.0);
        assert_eq!(metrics["heavy_tx_share"], 1.0 / 3.0);
        assert_eq!(metrics["heavy_gas_share"], 400_000.0 / 600_000.0);
        assert_eq!(metrics["calldata_bytes_per_tx"], 200.0);
        assert_eq!(metrics["logs_per_tx"], 3.0);
        assert_eq!(metrics["da_bytes_per_gas"], 400.0 / 600_000.0);
    }

    #[test]
    fn metrics_are_empty_without_gas() {
        assert!(TxProfile::metrics(&[]).is_empty());
    }

    #[test]
    fn compare_reports_error_and_tolerance_per_metric() {
        let targets = r#"{"metrics": {
            "gas_per_tx": {"target": 180000, "tolerance": 0.1},
            "logs_per_tx": {"target": 4, "tolerance": 0.25},
            "new_slots_per_gas": {"target": null, "tolerance": 0.25}
        }}"#;
        let txs = [tx(190_000, 0, 1, 0), tx(170_000, 0, 1, 0)];
        let calibration = Calibration::compare(targets, &txs).unwrap();
        assert_eq!(calibration.transactions, 2);
        assert_eq!(calibration.metrics["gas_per_tx"].error, Some(0.0));
        assert_eq!(calibration.metrics["gas_per_tx"].within_tolerance, Some(true));
        assert_eq!(calibration.metrics["logs_per_tx"].measured, Some(1.0));
        assert_eq!(calibration.metrics["logs_per_tx"].error, Some(-0.75));
        assert_eq!(calibration.metrics["logs_per_tx"].within_tolerance, Some(false));
        assert_eq!(
            calibration.metrics["new_slots_per_gas"],
            MetricCalibration {
                target: None,
                measured: None,
                error: None,
                tolerance: 0.25,
                within_tolerance: None
            }
        );
        assert!(!calibration.within_tolerance);
    }
}
