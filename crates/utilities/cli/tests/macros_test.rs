//! Integration tests for `define_metrics_args!` and `define_log_args!` macros.

use clap::{CommandFactory, Parser};

base_cli_utils::define_metrics_args!("TEST", 9090);

#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    metrics: MetricsArgs,
}

#[test]
fn metrics_env_var_prefix() {
    let cmd = TestCli::command();
    let args: Vec<_> = cmd.get_arguments().collect();

    let enabled_arg = args.iter().find(|a| a.get_long() == Some("metrics.enabled"));
    assert!(enabled_arg.is_some(), "metrics.enabled arg should exist");
    assert_eq!(
        enabled_arg.unwrap().get_env().map(|s| s.to_str().unwrap()),
        Some("TEST_METRICS_ENABLED"),
        "env var should use TEST_ prefix"
    );

    let port_arg = args.iter().find(|a| a.get_long() == Some("metrics.port"));
    assert!(port_arg.is_some(), "metrics.port arg should exist");
    assert_eq!(
        port_arg.unwrap().get_env().map(|s| s.to_str().unwrap()),
        Some("TEST_METRICS_PORT"),
        "env var should use TEST_ prefix"
    );
}

#[test]
fn metrics_default_port_is_9090() {
    let cli = TestCli::parse_from(["test"]);
    assert_eq!(cli.metrics.port, 9090);
}

#[test]
fn metrics_default_is_deterministic() {
    let a = MetricsArgs::default();
    let b = MetricsArgs::default();
    assert_eq!(a.enabled, b.enabled);
    assert_eq!(a.port, b.port);
    assert_eq!(a.addr, b.addr);
    assert_eq!(a.interval, b.interval);
    assert_eq!(a.interval, 30);
}

mod custom_port {
    use clap::{CommandFactory, Parser};

    base_cli_utils::define_metrics_args!("CUSTOM", 7300);

    #[derive(Parser)]
    struct CustomCli {
        #[command(flatten)]
        metrics: MetricsArgs,
    }

    #[test]
    fn metrics_custom_default_port() {
        let cli = CustomCli::parse_from(["test"]);
        assert_eq!(cli.metrics.port, 7300);
    }

    #[test]
    fn metrics_custom_env_var_prefix() {
        let cmd = CustomCli::command();
        let args: Vec<_> = cmd.get_arguments().collect();
        let enabled = args.iter().find(|a| a.get_long() == Some("metrics.enabled")).unwrap();
        assert_eq!(enabled.get_env().map(|s| s.to_str().unwrap()), Some("CUSTOM_METRICS_ENABLED"));
    }
}

mod log_args_tests {
    use base_cli_utils::{LogConfig, LogFormat};
    use clap::{CommandFactory, Parser};
    use tracing::level_filters::LevelFilter;

    base_cli_utils::define_log_args!("TEST");

    #[derive(Parser)]
    struct LogCli {
        #[command(flatten)]
        log: LogArgs,
    }

    #[test]
    fn log_env_var_prefix() {
        let cmd = LogCli::command();
        let args: Vec<_> = cmd.get_arguments().collect();

        let level_arg = args.iter().find(|a| a.get_long() == Some("verbose")).unwrap();
        assert_eq!(level_arg.get_env().map(|s| s.to_str().unwrap()), Some("TEST_LOG_VERBOSITY"),);

        let format_arg = args.iter().find(|a| a.get_long() == Some("logs.stdout.format")).unwrap();
        assert_eq!(format_arg.get_env().map(|s| s.to_str().unwrap()), Some("TEST_LOG_FORMAT"),);

        let dir_arg = args.iter().find(|a| a.get_long() == Some("logs.file.directory")).unwrap();
        assert_eq!(dir_arg.get_env().map(|s| s.to_str().unwrap()), Some("TEST_LOG_DIR"),);
    }

    #[test]
    fn log_defaults() {
        let cli = LogCli::parse_from(["test"]);
        assert_eq!(cli.log.level, 3);
        assert!(!cli.log.stdout_quiet);
        assert_eq!(cli.log.stdout_format, LogFormat::Full);
        assert!(cli.log.file_directory.is_none());
    }

    #[test]
    fn log_to_config_conversion() {
        let args = LogArgs::default();
        let config = LogConfig::from(args);
        assert_eq!(config.global_level, LevelFilter::INFO);
        assert!(config.stdout_logs.is_some());
        assert!(config.file_logs.is_none());
    }

    #[test]
    fn log_quiet_disables_stdout() {
        let args = LogArgs { stdout_quiet: true, ..Default::default() };
        let config = LogConfig::from(args);
        assert!(config.stdout_logs.is_none());
    }
}
