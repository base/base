//! Implementation of the `basectl doctor` subcommand.

use std::io::{self, Write};

use anyhow::{Result, bail};
use basectl_cli::{
    Doctor, DoctorCheck, DoctorOptions, DoctorReport, DoctorStatus, DoctorThresholds, JsonOutput,
    MonitoringConfig,
};

use crate::{cli::DoctorArgs, helpers::CommandOutcome};

const ANSI_RED: &str = "\x1b[31m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RESET: &str = "\x1b[0m";

/// Runs the `basectl doctor` subcommand.
pub(crate) async fn run(config: MonitoringConfig, args: DoctorArgs) -> Result<CommandOutcome> {
    validate_thresholds(&args)?;
    let options = DoctorOptions {
        el_rpc: args.el_rpc.unwrap_or_else(|| config.rpc.clone()),
        cl_rpc: args.cl_rpc.or_else(|| config.consensus_node_rpc.clone()),
        reth_config: args.reth_config,
        thresholds: DoctorThresholds {
            peer_warn_threshold: args.peer_warn_threshold,
            head_lag_warn_blocks: args.head_lag_warn_blocks,
            head_lag_fail_blocks: args.head_lag_fail_blocks,
            safe_recency_warn_blocks: args.safe_recency_warn_blocks,
            safe_recency_fail_blocks: args.safe_recency_fail_blocks,
        },
    };
    let json = args.json;
    let report = Doctor::run(config, options).await;
    if json {
        JsonOutput::print(&report)?;
    } else {
        print_pretty(&report)?;
    }
    Ok(CommandOutcome::from_failures(report.has_failures()))
}

fn validate_thresholds(args: &DoctorArgs) -> Result<()> {
    if args.head_lag_warn_blocks >= args.head_lag_fail_blocks {
        bail!("`--head-lag-warn-blocks` must be less than `--head-lag-fail-blocks`");
    }
    if args.safe_recency_warn_blocks >= args.safe_recency_fail_blocks {
        bail!("`--safe-recency-warn-blocks` must be less than `--safe-recency-fail-blocks`");
    }
    Ok(())
}

fn print_pretty(report: &DoctorReport) -> Result<()> {
    let mut stdout = io::stdout().lock();
    write_pretty(&mut stdout, report)?;
    Ok(())
}

fn write_pretty<W: Write>(writer: &mut W, report: &DoctorReport) -> io::Result<()> {
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
    for check in sorted_checks(report) {
        write_check(writer, check)?;
    }
    Ok(())
}

fn sorted_checks(report: &DoctorReport) -> Vec<&DoctorCheck> {
    let mut checks = report.checks.iter().collect::<Vec<_>>();
    checks.sort_by_key(|check| status_sort_key(check.status));
    checks
}

const fn status_sort_key(status: DoctorStatus) -> u8 {
    match status {
        DoctorStatus::Fail => 0,
        DoctorStatus::Warn => 1,
        DoctorStatus::Skip => 2,
        DoctorStatus::Info => 3,
        DoctorStatus::Pass => 4,
    }
}

fn write_check<W: Write>(writer: &mut W, check: &DoctorCheck) -> io::Result<()> {
    writeln!(writer, "{} {}", colored_status(check.status), check.check)?;
    writeln!(writer, "  message: {}", check.message)?;
    write_value_block(writer, "value", &check.value, 2)?;
    write_value_block(writer, "threshold", &check.threshold, 2)?;
    if let Some(hint) = &check.hint {
        writeln!(writer, "  hint: {hint}")?;
    }
    writeln!(writer)
}

fn colored_status(status: DoctorStatus) -> String {
    let color = match status {
        DoctorStatus::Fail => ANSI_RED,
        DoctorStatus::Warn => ANSI_YELLOW,
        DoctorStatus::Skip => ANSI_DIM,
        DoctorStatus::Info => ANSI_CYAN,
        DoctorStatus::Pass => ANSI_GREEN,
    };
    format!("{color}{}{ANSI_RESET}", status.as_str())
}

fn write_value_block<W: Write>(
    writer: &mut W,
    label: &str,
    value: &serde_json::Value,
    indent: usize,
) -> io::Result<()> {
    if is_empty_value(value) {
        return Ok(());
    }
    writeln!(writer, "{0:1$}{label}:", "", indent)?;
    write_json_value(writer, value, indent + 2)
}

fn write_json_value<W: Write>(
    writer: &mut W,
    value: &serde_json::Value,
    indent: usize,
) -> io::Result<()> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if is_empty_value(value) {
                    continue;
                }
                match value {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        writeln!(writer, "{0:1$}{key}:", "", indent)?;
                        write_json_value(writer, value, indent + 2)?;
                    }
                    _ => writeln!(
                        writer,
                        "{:indent$}{}: {}",
                        "",
                        key,
                        scalar_value(value),
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
                        write_json_value(writer, value, indent + 2)?;
                    }
                    _ => writeln!(
                        writer,
                        "{:indent$}- {}",
                        "",
                        scalar_value(value),
                        indent = indent,
                    )?,
                }
            }
            Ok(())
        }
        _ => writeln!(writer, "{:indent$}{}", "", scalar_value(value), indent = indent),
    }
}

fn scalar_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

fn is_empty_value(value: &serde_json::Value) -> bool {
    matches!(value, serde_json::Value::Null)
        || matches!(value, serde_json::Value::Object(map) if map.values().all(is_empty_value))
        || matches!(value, serde_json::Value::Array(values) if values.iter().all(is_empty_value))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use basectl_cli::{DoctorCheck, DoctorStatus};
    use serde_json::json;
    use url::Url;

    use super::{
        ANSI_CYAN, ANSI_GREEN, ANSI_RED, ANSI_YELLOW, colored_status, status_sort_key,
        validate_thresholds, write_check,
    };
    use crate::cli::DoctorArgs;

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

        write_check(&mut out, &check).unwrap();
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

        statuses.sort_by_key(|status| status_sort_key(*status));

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
    fn pretty_status_labels_are_colored() {
        assert_eq!(colored_status(DoctorStatus::Fail), format!("{ANSI_RED}FAIL\x1b[0m"));
        assert_eq!(colored_status(DoctorStatus::Warn), format!("{ANSI_YELLOW}WARN\x1b[0m"));
        assert_eq!(colored_status(DoctorStatus::Info), format!("{ANSI_CYAN}INFO\x1b[0m"));
        assert_eq!(colored_status(DoctorStatus::Pass), format!("{ANSI_GREEN}PASS\x1b[0m"));
    }

    #[test]
    fn rejects_invalid_head_lag_thresholds() {
        let args = test_args(|args| {
            args.head_lag_warn_blocks = 30;
            args.head_lag_fail_blocks = 10;
        });

        let err = validate_thresholds(&args).unwrap_err();

        assert!(err.to_string().contains("--head-lag-warn-blocks"));
    }

    #[test]
    fn rejects_invalid_safe_recency_thresholds() {
        let args = test_args(|args| {
            args.safe_recency_warn_blocks = 300;
            args.safe_recency_fail_blocks = 300;
        });

        let err = validate_thresholds(&args).unwrap_err();

        assert!(err.to_string().contains("--safe-recency-warn-blocks"));
    }

    fn test_args(update: impl FnOnce(&mut DoctorArgs)) -> DoctorArgs {
        let mut args = DoctorArgs {
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
