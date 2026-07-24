//! Implementation of the `basectl doctor` subcommand.

use std::{
    io::{self, Write},
    path::PathBuf,
};

use anyhow::Result;
use clap::Args;
use url::Url;

use crate::{
    CommandOutcome, Doctor, DoctorArgsError, DoctorCheck, DoctorOptions, DoctorReport,
    DoctorStatus, DoctorThresholds, JsonOutput, MonitoringConfig,
};

const ANSI_RED: &str = "\x1b[31m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RESET: &str = "\x1b[0m";

/// Arguments for running read-only diagnostics on a single node.
#[derive(Debug, Args)]
pub struct DoctorCommand {
    /// Override the execution-layer RPC URL.
    ///
    /// Defaults to the chain config's `rpc` field. Pass this flag to diagnose
    /// a specific node instead of a public preset RPC.
    #[arg(long = "el-rpc", value_name = "URL")]
    pub el_rpc: Option<Url>,
    /// Override the consensus-node RPC URL.
    ///
    /// If omitted and the selected config has no `consensus_node_rpc`, CL
    /// checks are skipped with hints while EL/L1/config checks still run.
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub cl_rpc: Option<Url>,
    /// Path to the local `reth.toml` file.
    #[arg(long = "reth-config", value_name = "PATH")]
    pub reth_config: Option<PathBuf>,
    /// Connected peer count below which peer checks warn.
    #[arg(long = "peer-warn-threshold", value_name = "COUNT", default_value_t = 5)]
    pub peer_warn_threshold: u32,
    /// EL head lag above which `el_head_vs_tip` warns.
    #[arg(long = "head-lag-warn-blocks", value_name = "BLOCKS", default_value_t = 10)]
    pub head_lag_warn_blocks: u64,
    /// EL head lag above which `el_head_vs_tip` fails.
    #[arg(long = "head-lag-fail-blocks", value_name = "BLOCKS", default_value_t = 20)]
    pub head_lag_fail_blocks: u64,
    /// Safe-head lag above which `safe_head_recency` warns.
    #[arg(long = "safe-recency-warn-blocks", value_name = "BLOCKS", default_value_t = 150)]
    pub safe_recency_warn_blocks: u64,
    /// Safe-head lag above which `safe_head_recency` fails.
    #[arg(long = "safe-recency-fail-blocks", value_name = "BLOCKS", default_value_t = 300)]
    pub safe_recency_fail_blocks: u64,
    /// Emit a humanized JSON report instead of pretty text.
    #[arg(long)]
    pub json: bool,
}

impl DoctorCommand {
    /// Runs diagnostics and renders the selected output format.
    pub async fn run(self, config: MonitoringConfig) -> Result<CommandOutcome> {
        self.validate_thresholds()?;
        let options = DoctorOptions {
            el_rpc: self.el_rpc.unwrap_or_else(|| config.rpc.clone()),
            cl_rpc: self.cl_rpc.or_else(|| config.consensus_node_rpc.clone()),
            reth_config: self.reth_config,
            thresholds: DoctorThresholds {
                peer_warn_threshold: self.peer_warn_threshold,
                head_lag_warn_blocks: self.head_lag_warn_blocks,
                head_lag_fail_blocks: self.head_lag_fail_blocks,
                safe_recency_warn_blocks: self.safe_recency_warn_blocks,
                safe_recency_fail_blocks: self.safe_recency_fail_blocks,
            },
        };
        let report = Doctor::run(config, options).await;
        if self.json {
            JsonOutput::print(&report)?;
        } else {
            Self::print_pretty(&report)?;
        }
        Ok(CommandOutcome::from_failures(report.has_failures()))
    }

    /// Validates that warning thresholds are lower than failure thresholds.
    pub const fn validate_thresholds(&self) -> Result<(), DoctorArgsError> {
        if self.head_lag_warn_blocks >= self.head_lag_fail_blocks {
            return Err(DoctorArgsError::HeadLagWarnMustBeLessThanFail {
                warn_blocks: self.head_lag_warn_blocks,
                fail_blocks: self.head_lag_fail_blocks,
            });
        }
        if self.safe_recency_warn_blocks >= self.safe_recency_fail_blocks {
            return Err(DoctorArgsError::SafeRecencyWarnMustBeLessThanFail {
                warn_blocks: self.safe_recency_warn_blocks,
                fail_blocks: self.safe_recency_fail_blocks,
            });
        }
        Ok(())
    }

    /// Writes pretty output to standard output.
    pub fn print_pretty(report: &DoctorReport) -> Result<()> {
        let mut stdout = io::stdout().lock();
        Self::write_pretty(&mut stdout, report)?;
        Ok(())
    }

    /// Writes pretty output to an arbitrary writer.
    pub fn write_pretty<W: Write>(writer: &mut W, report: &DoctorReport) -> io::Result<()> {
        writeln!(writer, "network  {}", report.network)?;
        writeln!(
            writer,
            "summary  pass={} warn={} fail={} info={} skip={}",
            report.summary.pass,
            report.summary.warn,
            report.summary.fail,
            report.summary.info,
            report.summary.skip,
        )?;
        writeln!(writer)?;
        for check in Self::sorted_checks(report) {
            Self::write_check(writer, check)?;
        }
        Ok(())
    }

    /// Returns checks ordered by operational severity.
    pub fn sorted_checks(report: &DoctorReport) -> Vec<&DoctorCheck> {
        let mut checks = report.checks.iter().collect::<Vec<_>>();
        checks.sort_by_key(|check| Self::status_sort_key(check.status));
        checks
    }

    /// Returns the sort priority for a diagnostic status.
    pub const fn status_sort_key(status: DoctorStatus) -> u8 {
        match status {
            DoctorStatus::Fail => 0,
            DoctorStatus::Warn => 1,
            DoctorStatus::Skip => 2,
            DoctorStatus::Info => 3,
            DoctorStatus::Pass => 4,
        }
    }

    /// Writes one diagnostic check.
    pub fn write_check<W: Write>(writer: &mut W, check: &DoctorCheck) -> io::Result<()> {
        writeln!(writer, "{} {}", Self::colored_status(check.status), check.check)?;
        writeln!(writer, "  message: {}", check.message)?;
        Self::write_value_block(writer, "value", &check.value, 2)?;
        Self::write_value_block(writer, "threshold", &check.threshold, 2)?;
        if let Some(hint) = &check.hint {
            writeln!(writer, "  hint: {hint}")?;
        }
        writeln!(writer)
    }

    /// Formats a diagnostic status with its ANSI color.
    pub fn colored_status(status: DoctorStatus) -> String {
        let color = match status {
            DoctorStatus::Fail => ANSI_RED,
            DoctorStatus::Warn => ANSI_YELLOW,
            DoctorStatus::Skip => ANSI_DIM,
            DoctorStatus::Info => ANSI_CYAN,
            DoctorStatus::Pass => ANSI_GREEN,
        };
        format!("{color}{}{ANSI_RESET}", status.as_str())
    }

    /// Writes a labeled JSON value block when it is non-empty.
    pub fn write_value_block<W: Write>(
        writer: &mut W,
        label: &str,
        value: &serde_json::Value,
        indent: usize,
    ) -> io::Result<()> {
        if Self::is_empty_value(value) {
            return Ok(());
        }
        writeln!(writer, "{0:1$}{label}:", "", indent)?;
        Self::write_json_value(writer, value, indent + 2)
    }

    /// Recursively writes a JSON value as indented text.
    pub fn write_json_value<W: Write>(
        writer: &mut W,
        value: &serde_json::Value,
        indent: usize,
    ) -> io::Result<()> {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if Self::is_empty_value(value) {
                        continue;
                    }
                    match value {
                        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                            writeln!(writer, "{0:1$}{key}:", "", indent)?;
                            Self::write_json_value(writer, value, indent + 2)?;
                        }
                        _ => writeln!(
                            writer,
                            "{:indent$}{}: {}",
                            "",
                            key,
                            Self::scalar_value(value),
                            indent = indent,
                        )?,
                    }
                }
                Ok(())
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    match value {
                        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                            writeln!(writer, "{0:1$}-", "", indent)?;
                            Self::write_json_value(writer, value, indent + 2)?;
                        }
                        _ => writeln!(
                            writer,
                            "{:indent$}- {}",
                            "",
                            Self::scalar_value(value),
                            indent = indent,
                        )?,
                    }
                }
                Ok(())
            }
            _ => writeln!(writer, "{:indent$}{}", "", Self::scalar_value(value), indent = indent),
        }
    }

    /// Formats a scalar JSON value without quoting strings.
    pub fn scalar_value(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::String(s) => s.clone(),
            _ => value.to_string(),
        }
    }

    /// Returns whether a JSON value contains no renderable data.
    pub fn is_empty_value(value: &serde_json::Value) -> bool {
        matches!(value, serde_json::Value::Null)
            || matches!(value, serde_json::Value::Object(map) if map.values().all(Self::is_empty_value))
            || matches!(value, serde_json::Value::Array(values) if values.iter().all(Self::is_empty_value))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use url::Url;

    use super::{ANSI_YELLOW, DoctorCommand};
    use crate::{DoctorArgsError, DoctorCheck, DoctorStatus};

    #[test]
    fn pretty_check_includes_status_value_threshold_and_hint() {
        let check = DoctorCheck::new(
            "el_peer_count",
            DoctorStatus::Warn,
            "EL peer count is below the warning threshold",
            json!({ "count": 3 }),
            json!({ "warnBelow": 5 }),
            Some("Check p2p config.".to_string()),
        );
        let mut out = Vec::new();

        DoctorCommand::write_check(&mut out, &check).unwrap();
        let rendered = String::from_utf8(out).unwrap();

        assert!(rendered.contains("WARN"));
        assert!(rendered.contains("el_peer_count"));
        assert!(rendered.contains(ANSI_YELLOW));
        assert!(rendered.contains("  value:\n    count: 3"));
        assert!(rendered.contains("  threshold:\n    warnBelow: 5"));
        assert!(rendered.contains("Check p2p config."));
    }

    #[test]
    fn pretty_status_order_prioritizes_actionable_rows() {
        let mut statuses = vec![
            DoctorStatus::Pass,
            DoctorStatus::Info,
            DoctorStatus::Warn,
            DoctorStatus::Skip,
            DoctorStatus::Fail,
        ];

        statuses.sort_by_key(|status| DoctorCommand::status_sort_key(*status));

        assert_eq!(
            statuses,
            vec![
                DoctorStatus::Fail,
                DoctorStatus::Warn,
                DoctorStatus::Skip,
                DoctorStatus::Info,
                DoctorStatus::Pass,
            ],
        );
    }

    #[test]
    fn rejects_invalid_head_lag_thresholds() {
        let args = test_args(|args| {
            args.head_lag_warn_blocks = 30;
            args.head_lag_fail_blocks = 10;
        });

        let err = args.validate_thresholds().unwrap_err();

        assert!(matches!(
            err,
            DoctorArgsError::HeadLagWarnMustBeLessThanFail { warn_blocks: 30, fail_blocks: 10 }
        ));
    }

    #[test]
    fn rejects_invalid_safe_recency_thresholds() {
        let args = test_args(|args| {
            args.safe_recency_warn_blocks = 300;
            args.safe_recency_fail_blocks = 300;
        });

        let err = args.validate_thresholds().unwrap_err();

        assert!(matches!(
            err,
            DoctorArgsError::SafeRecencyWarnMustBeLessThanFail {
                warn_blocks: 300,
                fail_blocks: 300,
            }
        ));
    }

    fn test_args(update: impl FnOnce(&mut DoctorCommand)) -> DoctorCommand {
        let mut args = DoctorCommand {
            el_rpc: Some(Url::parse("http://127.0.0.1:8545").unwrap()),
            cl_rpc: None,
            reth_config: Option::<PathBuf>::None,
            peer_warn_threshold: 5,
            head_lag_warn_blocks: 10,
            head_lag_fail_blocks: 20,
            safe_recency_warn_blocks: 150,
            safe_recency_fail_blocks: 300,
            json: false,
        };
        update(&mut args);
        args
    }
}
