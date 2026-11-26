#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use base_batcher_cli::BatcherArgs;
use base_cli_utils::{LogConfig, MetricsConfig, RuntimeManager};
use clap::Parser;
use eyre::WrapErr;
use reth_node_core::args::TraceArgs;

base_cli_utils::define_log_args!("BASE_NODE");
base_cli_utils::define_metrics_args!("BASE_NODE", 9090);

/// Standalone Base batcher CLI.
#[derive(Parser, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    styles = base_cli_utils::CliStyles::init(),
    about = "Submit L2 batch data to L1",
    long_about = None
)]
struct BatcherCli {
    /// Logging configuration.
    #[command(flatten)]
    logging: LogArgs,

    /// `OpenTelemetry` tracing export configuration.
    #[command(flatten)]
    traces: TraceArgs,

    /// Metrics configuration.
    #[command(flatten)]
    metrics: MetricsArgs,

    /// Batcher configuration.
    #[command(flatten)]
    batcher: BatcherArgs,
}

impl BatcherCli {
    fn run(self) -> eyre::Result<()> {
        // Tonic captures the runtime during OTLP initialization. Keep it alive while the
        // command runs, but leave its context before the batcher enters its own runtime.
        let tracing_runtime = self
            .traces
            .otlp
            .as_ref()
            .map(|_| {
                tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build()
            })
            .transpose()
            .wrap_err("failed to create tracing runtime")?;
        {
            let _guard = tracing_runtime.as_ref().map(tokio::runtime::Runtime::enter);
            LogConfig::from(self.logging)
                .init_with_trace_args(&self.traces, &[])
                .wrap_err("failed to initialize tracing")?;
        }

        let metrics_enabled = self.metrics.enabled;
        MetricsConfig::from(self.metrics)
            .init_with_builder(base_batcher_cli::configure_prometheus)
            .wrap_err("failed to install Prometheus recorder")?;
        if metrics_enabled {
            base_cli_utils::register_version_metrics!();
        }

        RuntimeManager::new()
            .run_until_shutdown(|cancel| self.batcher.exec(metrics_enabled, cancel))
    }
}

fn main() -> eyre::Result<()> {
    base_cli_utils::init_common!();
    BatcherCli::parse().run()
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::BatcherCli;

    #[test]
    fn parses_batcher_logging_and_metrics_flags() {
        let cli = BatcherCli::parse_from([
            "base-batcher",
            "--l1-rpc-url",
            "http://127.0.0.1:8545",
            "--sequencer-urls",
            "http://127.0.0.1:9545",
            "--private-key",
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            "--metrics.enabled",
            "--metrics.port",
            "9191",
            "--logs.stdout.format",
            "json",
            "--tracing-otlp=http://127.0.0.1:4318",
        ]);
        assert!(cli.metrics.enabled);
        assert_eq!(cli.metrics.port, 9191);
        assert_eq!(cli.batcher.sequencer_urls.len(), 1);
        assert!(cli.traces.otlp.is_some());
    }
}
