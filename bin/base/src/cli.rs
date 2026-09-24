use base_cli_utils::{LogConfig, MetricsConfig};
use clap::Parser;
use eyre::WrapErr;
use reth_node_core::args::TraceArgs;

use crate::{
    commands::BaseCommand,
    config::{ChainArg, ChainResolver},
};

base_cli_utils::define_log_args!("BASE_NODE");
base_cli_utils::define_metrics_args!("BASE_NODE", 9090);

/// The `base` CLI.
#[derive(Parser, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    styles = base_cli_utils::CliStyles::init(),
    about,
    long_about = None
)]
pub(crate) struct BaseCli {
    /// Chain selection.
    ///
    /// Uses a distinct clap `id` so nested reth-derived subcommands (e.g. `base reth db`) can
    /// register their own globally-propagated `--chain` arg without colliding at value-access
    /// time in [`FromArgMatches`].
    #[arg(id = "base_chain", long = "chain", short = 'c', env = "BASE_CHAIN")]
    pub(crate) chain: Option<ChainArg>,

    /// Logging configuration.
    #[command(flatten)]
    pub(crate) logging: LogArgs,

    /// `OpenTelemetry` tracing export configuration.
    #[command(flatten)]
    pub traces: TraceArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub(crate) metrics: MetricsArgs,

    /// The command to run.
    #[command(subcommand)]
    pub(crate) command: BaseCommand,
}

impl BaseCli {
    /// Runs the selected command with shared process initialization.
    pub(crate) fn run(self) -> eyre::Result<()> {
        // Tonic captures the runtime during OTLP initialization. Keep it alive while the
        // command runs, but leave its context before commands enter their own runtimes.
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

        self.command.run(ChainResolver::new(self.chain), metrics_enabled)
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, process::Command, time::Duration};

    use axum::{Router, body::Bytes, routing::post};
    use clap::{CommandFactory, Parser};
    use tokio::{
        net::TcpListener,
        sync::{mpsc, oneshot},
    };

    use super::*;

    #[test]
    fn parses_shared_otlp_configuration() {
        for flavor in ["rpc", "follow", "sequencer"] {
            for before_subcommand in [false, true] {
                let mut args = vec!["base"];
                if !before_subcommand {
                    args.push(flavor);
                }
                args.extend([
                    "--tracing-otlp=http://localhost:4317",
                    "--tracing-otlp-protocol",
                    "grpc",
                    "--tracing-otlp.filter",
                    "warn,base_otlp_test=debug",
                    "--tracing-otlp.service-name",
                    "unified-test",
                    "--tracing-otlp.sample-ratio",
                    "0.25",
                ]);
                if before_subcommand {
                    args.push(flavor);
                }
                args.extend([
                    "--l1-eth-rpc",
                    "http://localhost:8545",
                    "--l1-beacon",
                    "http://localhost:5052",
                ]);
                if flavor == "follow" {
                    args.extend(["--source-l2-rpc", "http://localhost:9545"]);
                } else if flavor == "sequencer" {
                    args.extend(["--p2p.sequencer.key.path", "/tmp/sequencer-key"]);
                }
                let cli = BaseCli::try_parse_from(args).unwrap();
                assert_eq!(cli.traces.otlp.unwrap().as_str(), "http://localhost:4317/");
                assert_eq!(cli.traces.service_name, "unified-test");
                assert_eq!(cli.traces.sample_ratio, Some(0.25));
                assert_eq!(cli.traces.otlp_filter.to_string(), "base_otlp_test=debug,warn");
            }
        }
    }

    #[test]
    fn exports_otlp_spans_with_independent_filter() {
        // Each exporter needs its own process because the tracing subscriber is global.
        for protocol in ["http", "grpc"] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            for (key, _) in std::env::vars() {
                if key.starts_with("OTEL_") || key.starts_with("BASE_") || key == "RUST_LOG" {
                    command.env_remove(key);
                }
            }
            let output = command
                .args(["--exact", "cli::tests::exports_otlp_child", "--ignored", "--nocapture"])
                .env("BASE_OTLP_TEST_PROTOCOL", protocol)
                .env("OTEL_BSP_SCHEDULE_DELAY", "50")
                .env("OTEL_SERVICE_NAME", "unified-otlp-test")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{protocol} export failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    #[tokio::test]
    #[ignore = "spawned by exports_otlp_spans_with_independent_filter"]
    async fn exports_otlp_child() {
        let protocol = std::env::var("BASE_OTLP_TEST_PROTOCOL").unwrap();
        let collector = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", collector.local_addr().unwrap());
        let (sender, mut requests) = mpsc::unbounded_channel();
        let grpc = protocol == "grpc";
        let path = if grpc {
            "/opentelemetry.proto.collector.trace.v1.TraceService/Export"
        } else {
            "/v1/traces"
        };
        let router = Router::new().route(
            path,
            post(move |body: Bytes| {
                let sender = sender.clone();
                async move {
                    sender.send(body).unwrap();
                    let content_type =
                        if grpc { "application/grpc" } else { "application/x-protobuf" };
                    // An empty ExportTraceServiceResponse, with a gRPC message envelope if needed.
                    (
                        [("content-type", content_type), ("grpc-status", "0")],
                        if grpc { vec![0_u8; 5] } else { Vec::new() },
                    )
                }
            }),
        );
        let (stop_collector, collector_shutdown) = oneshot::channel();
        let collector = tokio::spawn(async move {
            axum::serve(collector, router)
                .with_graceful_shutdown(async {
                    let _ = collector_shutdown.await;
                })
                .await
                .unwrap();
        });

        let otlp_arg = format!("--tracing-otlp={endpoint}");
        let cli = BaseCli::try_parse_from([
            "base",
            "--chain",
            "dev",
            "rpc",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
            "-q",
            &otlp_arg,
            "--tracing-otlp-protocol",
            &protocol,
            "--tracing-otlp.filter",
            "off,base_otlp_test=debug",
        ])
        .unwrap();
        LogConfig::from(cli.logging).init_with_trace_args(&cli.traces, &[]).unwrap();
        let received = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                {
                    let _span =
                        tracing::debug_span!(target: "base_otlp_test", "included_span").entered();
                }
                {
                    let _span =
                        tracing::error_span!(target: "excluded_target", "excluded_span").entered();
                }
                tokio::select! {
                    Some(body) = requests.recv() => break body,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        })
        .await
        .expect("timed out waiting for OTLP export");
        stop_collector.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), collector)
            .await
            .expect("timed out waiting for collector shutdown")
            .unwrap();

        // Protobuf string fields retain their UTF-8 bytes in both transport encodings.
        let payload = String::from_utf8_lossy(&received);
        assert!(payload.contains("included_span"), "missing enabled debug span: {payload}");
        assert!(payload.contains("unified-otlp-test"), "missing service name: {payload}");
        assert!(!payload.contains("excluded_span"), "OTLP filter was ignored: {payload}");
    }

    #[test]
    fn parses_batcher_configuration() {
        let cli = BaseCli::try_parse_from([
            "base",
            "batcher",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--l2-rpc-url",
            "http://localhost:9545",
            "--rollup-rpc-url",
            "http://localhost:7545",
            "--signer-endpoint",
            "http://localhost:9000",
            "--signer-address",
            "0x4242424242424242424242424242424242424242",
            "--metrics.enabled",
            "--metrics.port",
            "7301",
            "--data-availability-type",
            "calldata",
            "--stopped",
        ])
        .unwrap();
        let BaseCommand::Batcher(batcher) = cli.command else {
            panic!("expected batcher command");
        };
        assert_eq!(cli.metrics.port, 7301);
        let config = batcher.into_config(cli.metrics.enabled).unwrap();
        assert_eq!(config.l1_rpc_url[0].as_str(), "http://localhost:8545/");
        assert!(config.metrics_enabled);
        assert!(config.stopped);
    }

    #[test]
    fn batcher_uses_shared_observability_settings() {
        let cli = BaseCli::try_parse_from(["base", "batcher"]).unwrap();
        assert_eq!(cli.metrics.port, 9090);
        let command = BaseCli::command();
        for (flag, env) in [
            ("metrics.port", "BASE_NODE_METRICS_PORT"),
            ("logs.stdout.format", "BASE_NODE_LOG_FORMAT"),
        ] {
            let arg = command.get_arguments().find(|arg| arg.get_long() == Some(flag)).unwrap();
            assert_eq!(arg.get_env(), Some(OsStr::new(env)));
        }
    }

    #[test]
    fn batcher_uses_l1_rpc_url_and_accepts_aliases() {
        for flag in ["--l1-eth-rpc", "--l1-rpc-url", "--l1"] {
            let cli = BaseCli::try_parse_from(["base", "batcher", flag, "http://localhost:8545"])
                .unwrap();
            let BaseCommand::Batcher(batcher) = cli.command else {
                panic!("expected batcher");
            };
            assert_eq!(batcher.l1_rpc_url[0].as_str(), "http://localhost:8545/");
        }
        let command = BaseCli::command();
        let batcher = command.find_subcommand("batcher").unwrap();
        for (flag, env) in [
            ("l1-rpc-url", "BASE_NODE_L1_ETH_RPC"),
            ("private-key", "BASE_BATCHER_PRIVATE_KEY"),
            ("rollup-rpc-url", "BASE_BATCHER_ROLLUP_RPC_URL"),
        ] {
            let arg = batcher.get_arguments().find(|arg| arg.get_long() == Some(flag)).unwrap();
            assert_eq!(arg.get_env(), Some(OsStr::new(env)));
        }
    }

    #[test]
    fn batcher_rejects_top_level_chain_selection() {
        let cli = BaseCli::try_parse_from(["base", "--chain", "sepolia", "batcher"]).unwrap();
        let error = cli.command.run(ChainResolver::new(cli.chain), false).unwrap_err();
        assert!(error.to_string().contains("`base batcher` manages its own chain configuration"));
    }

    #[test]
    fn parses_default_chain_for_rpc() {
        let cli = BaseCli::parse_from([
            "base",
            "rpc",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l1-beacon",
            "http://localhost:5052",
        ]);

        assert_eq!(cli.chain, None);
        assert!(matches!(cli.command, BaseCommand::Rpc(_)));
    }

    #[test]
    fn parses_named_chain_selector() {
        let cli = BaseCli::parse_from(["base", "-c", "sepolia", "bootnode"]);

        assert!(matches!(cli.chain, Some(ChainArg::BuiltIn(ref name)) if name == "sepolia"));
    }

    #[test]
    fn rejects_chain_after_subcommand() {
        // `--chain` is no longer globally propagated so nested reth subcommands can register
        // their own `--chain` arg without clap `Long option names must be unique` collisions.
        // Callers must supply `--chain` before the subcommand: `base --chain sepolia bootnode`.
        let err = BaseCli::try_parse_from(["base", "bootnode", "--chain", "sepolia"]).unwrap_err();

        assert!(err.to_string().contains("unexpected argument '--chain'"));
    }

    #[test]
    fn parses_path_chain_selector() {
        let cli = BaseCli::parse_from(["base", "--chain", "./chain.toml", "bootnode"]);

        assert!(matches!(cli.chain, Some(ChainArg::File(_))));
    }

    #[test]
    fn chain_arg_uses_base_chain_env_var() {
        let command = BaseCli::command();
        let chain_arg =
            command.get_arguments().find(|arg| arg.get_long() == Some("chain")).unwrap();

        assert_eq!(chain_arg.get_env(), Some(OsStr::new("BASE_CHAIN")));
    }

    #[test]
    fn rejects_multiple_chain_selectors() {
        let err =
            BaseCli::try_parse_from(["base", "-c", "mainnet", "--chain", "sepolia", "bootnode"])
                .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("cannot be used multiple times"));
    }

    #[test]
    fn preserves_base_chain_alongside_reth_subcommand_chain() {
        let cli =
            BaseCli::try_parse_from(["base", "--chain", "sepolia", "reth", "db", "stats"]).unwrap();

        assert!(matches!(cli.chain, Some(ChainArg::BuiltIn(ref name)) if name == "sepolia"));
        assert!(matches!(cli.command, BaseCommand::Reth(_)));
    }
}
