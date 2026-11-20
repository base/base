//! `OpenTelemetry` OTLP tracing and logging CLI arguments.
//!
//! Flag names, environment variables, and defaults mirror `reth_node_core::args::TraceArgs` so
//! non-execution binaries can expose identical OTLP configuration without linking `reth-node-core`.

use std::sync::OnceLock;

use clap::{Parser, ValueEnum, builder::Resettable};
use eyre::{WrapErr, ensure};
use tracing_subscriber::EnvFilter;
use url::Url;

const HTTP_TRACE_ENDPOINT: &str = "/v1/traces";
const HTTP_LOGS_ENDPOINT: &str = "/v1/logs";

static TRACE_DEFAULTS: OnceLock<DefaultTraceValues> = OnceLock::new();

/// OTLP transport protocol for exporting traces and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum OtlpProtocol {
    /// HTTP/Protobuf transport, port 4318, requires `/v1/traces` or `/v1/logs` path.
    #[default]
    Http,
    /// gRPC transport, port 4317.
    Grpc,
}

impl OtlpProtocol {
    /// Validates and normalizes `url` for OTLP trace export.
    pub fn validate_endpoint(self, url: &mut Url) -> eyre::Result<()> {
        self.validate_endpoint_with_path(url, HTTP_TRACE_ENDPOINT)
    }

    /// Validates and normalizes `url` for OTLP log export.
    pub fn validate_logs_endpoint(self, url: &mut Url) -> eyre::Result<()> {
        self.validate_endpoint_with_path(url, HTTP_LOGS_ENDPOINT)
    }

    fn validate_endpoint_with_path(self, url: &mut Url, http_path: &str) -> eyre::Result<()> {
        match self {
            Self::Http => {
                if !url.path().ends_with(http_path) {
                    let path = url.path().trim_end_matches('/');
                    url.set_path(&format!("{path}{http_path}"));
                }
            }
            Self::Grpc => {
                ensure!(
                    !url.path().ends_with(http_path),
                    "OTLP gRPC endpoint should not include {http_path} path, got: {url}"
                );
            }
        }
        Ok(())
    }

    #[cfg(feature = "otlp")]
    pub(crate) const fn into_reth(self) -> reth_tracing_otlp::OtlpProtocol {
        match self {
            Self::Http => reth_tracing_otlp::OtlpProtocol::Http,
            Self::Grpc => reth_tracing_otlp::OtlpProtocol::Grpc,
        }
    }
}

/// Overridable defaults for [`TraceArgs`].
///
/// Binaries can call `DefaultTraceValues::default().with_service_name("myapp").try_init()` before
/// CLI parsing to customize the defaults clap exposes.
#[derive(Debug, Clone)]
pub struct DefaultTraceValues {
    otlp: Option<Url>,
    logs_otlp: Option<Url>,
    otlp_default_endpoint: String,
    logs_otlp_default_endpoint: String,
    protocol: OtlpProtocol,
    service_name: String,
    service_version: Option<String>,
    otlp_filter: String,
    logs_otlp_filter: String,
    sample_ratio: Option<f64>,
    sample_ratio_str: Option<String>,
}

impl Default for DefaultTraceValues {
    fn default() -> Self {
        Self {
            otlp: None,
            logs_otlp: None,
            otlp_default_endpoint: "http://localhost:4318/v1/traces".to_string(),
            logs_otlp_default_endpoint: "http://localhost:4318/v1/logs".to_string(),
            protocol: OtlpProtocol::Http,
            service_name: "reth".to_string(),
            service_version: None,
            otlp_filter: "debug".to_string(),
            logs_otlp_filter: "info".to_string(),
            sample_ratio: None,
            sample_ratio_str: None,
        }
    }
}

impl DefaultTraceValues {
    /// Initializes the global trace defaults with this configuration.
    pub fn try_init(self) -> Result<(), Box<Self>> {
        TRACE_DEFAULTS.set(self).map_err(Box::new)
    }

    /// Returns a reference to the global trace defaults.
    pub fn get_global() -> &'static Self {
        TRACE_DEFAULTS.get_or_init(Self::default)
    }

    /// Sets the default OTLP tracing endpoint.
    pub fn with_otlp(mut self, otlp: Option<Url>) -> Self {
        self.otlp = otlp;
        self
    }

    /// Sets the default OTLP logs endpoint.
    pub fn with_logs_otlp(mut self, logs_otlp: Option<Url>) -> Self {
        self.logs_otlp = logs_otlp;
        self
    }

    /// Sets the endpoint used when `--tracing-otlp` is provided without a value.
    pub fn with_otlp_default_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.otlp_default_endpoint = endpoint.into();
        self
    }

    /// Sets the endpoint used when `--logs-otlp` is provided without a value.
    pub fn with_logs_otlp_default_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.logs_otlp_default_endpoint = endpoint.into();
        self
    }

    /// Sets the default OTLP transport protocol.
    pub const fn with_protocol(mut self, protocol: OtlpProtocol) -> Self {
        self.protocol = protocol;
        self
    }

    /// Sets the default service name.
    pub fn with_service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = name.into();
        self
    }

    /// Sets the default service version.
    pub fn with_service_version(mut self, version: impl Into<String>) -> Self {
        self.service_version = Some(version.into());
        self
    }

    /// Sets the default OTLP tracing filter.
    pub fn with_otlp_filter(mut self, filter: impl Into<String>) -> Self {
        self.otlp_filter = filter.into();
        self
    }

    /// Sets the default OTLP logs filter.
    pub fn with_logs_otlp_filter(mut self, filter: impl Into<String>) -> Self {
        self.logs_otlp_filter = filter.into();
        self
    }

    /// Sets the default OTLP trace sampling ratio.
    pub fn with_sample_ratio(mut self, sample_ratio: Option<f64>) -> Self {
        self.sample_ratio_str = sample_ratio.map(|ratio| ratio.to_string());
        self.sample_ratio = sample_ratio;
        self
    }

    const fn protocol_as_str(&self) -> &'static str {
        match self.protocol {
            OtlpProtocol::Http => "http",
            OtlpProtocol::Grpc => "grpc",
        }
    }
}

/// CLI arguments for configuring `OpenTelemetry` trace and logs export.
#[derive(Debug, Clone, Parser)]
pub struct TraceArgs {
    /// Enable `OpenTelemetry` tracing export to an OTLP endpoint.
    ///
    /// If no value provided, defaults based on protocol:
    /// - HTTP: `http://localhost:4318/v1/traces`
    /// - gRPC: `http://localhost:4317`
    ///
    /// Example: `--tracing-otlp=http://collector:4318/v1/traces`
    #[arg(
        long = "tracing-otlp",
        env = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        global = true,
        value_name = "URL",
        num_args = 0..=1,
        default_value = Resettable::from(DefaultTraceValues::get_global().otlp.as_ref().map(|url| url.as_str().into())),
        default_missing_value = DefaultTraceValues::get_global().otlp_default_endpoint.as_str(),
        require_equals = true,
        value_parser = parse_otlp_endpoint,
        help_heading = "Tracing"
    )]
    pub otlp: Option<Url>,

    /// Enable `OpenTelemetry` logs export to an OTLP endpoint.
    ///
    /// If no value provided, defaults based on protocol:
    /// - HTTP: `http://localhost:4318/v1/logs`
    /// - gRPC: `http://localhost:4317`
    ///
    /// Example: `--logs-otlp=http://collector:4318/v1/logs`
    #[arg(
        long = "logs-otlp",
        env = "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        global = true,
        value_name = "URL",
        num_args = 0..=1,
        default_value = Resettable::from(DefaultTraceValues::get_global().logs_otlp.as_ref().map(|url| url.as_str().into())),
        default_missing_value = DefaultTraceValues::get_global().logs_otlp_default_endpoint.as_str(),
        require_equals = true,
        value_parser = parse_otlp_endpoint,
        help_heading = "Logging"
    )]
    pub logs_otlp: Option<Url>,

    /// OTLP transport protocol to use for exporting traces and logs.
    ///
    /// - `http`: expects endpoint path to end with `/v1/traces` or `/v1/logs`
    /// - `grpc`: expects endpoint without a path
    ///
    /// Defaults to HTTP if not specified.
    #[arg(
        long = "tracing-otlp-protocol",
        env = "OTEL_EXPORTER_OTLP_PROTOCOL",
        global = true,
        value_name = "PROTOCOL",
        default_value = DefaultTraceValues::get_global().protocol_as_str(),
        help_heading = "Tracing"
    )]
    pub protocol: OtlpProtocol,

    /// Set a filter directive for the OTLP tracer. This controls the verbosity
    /// of spans and events sent to the OTLP endpoint. It follows the same
    /// syntax as the `RUST_LOG` environment variable.
    ///
    /// Example: `--tracing-otlp.filter=info,reth=debug,hyper_util=off`
    #[arg(
        long = "tracing-otlp.filter",
        global = true,
        value_name = "FILTER",
        default_value = DefaultTraceValues::get_global().otlp_filter.as_str(),
        help_heading = "Tracing"
    )]
    pub otlp_filter: EnvFilter,

    /// Set a filter directive for the OTLP logs exporter. This controls the verbosity
    /// of logs sent to the OTLP endpoint. It follows the same syntax as the
    /// `RUST_LOG` environment variable.
    ///
    /// Example: `--logs-otlp.filter=info,reth=debug`
    #[arg(
        long = "logs-otlp.filter",
        global = true,
        value_name = "FILTER",
        default_value = DefaultTraceValues::get_global().logs_otlp_filter.as_str(),
        help_heading = "Logging"
    )]
    pub logs_otlp_filter: EnvFilter,

    /// Service name to use for OTLP tracing export.
    ///
    /// Set via `OTEL_SERVICE_NAME` environment variable. Defaults to "reth" if not specified.
    #[arg(
        long = "tracing-otlp.service-name",
        env = "OTEL_SERVICE_NAME",
        global = true,
        value_name = "NAME",
        default_value = DefaultTraceValues::get_global().service_name.as_str(),
        hide = true,
        help_heading = "Tracing"
    )]
    pub service_name: String,

    /// Service version to use for OTLP tracing export.
    ///
    /// Overrides the default version reported in the `service.version` OTLP resource attribute.
    /// Falls back to the crate's `CARGO_PKG_VERSION` if not specified.
    #[arg(
        long = "tracing-otlp.service-version",
        env = "OTEL_SERVICE_VERSION",
        global = true,
        value_name = "VERSION",
        default_value = Resettable::from(DefaultTraceValues::get_global().service_version.as_ref().map(|version| version.as_str().into())),
        hide = true,
        help_heading = "Tracing"
    )]
    pub service_version: Option<String>,

    /// Trace sampling ratio to control the percentage of traces to export.
    ///
    /// Valid range: 0.0 to 1.0.
    ///
    /// Example: `--tracing-otlp.sample-ratio=0.0`
    #[arg(
        long = "tracing-otlp.sample-ratio",
        env = "OTEL_TRACES_SAMPLER_ARG",
        global = true,
        value_name = "RATIO",
        default_value = Resettable::from(DefaultTraceValues::get_global().sample_ratio_str.as_deref().map(Into::into)),
        help_heading = "Tracing"
    )]
    pub sample_ratio: Option<f64>,
}

impl Default for TraceArgs {
    fn default() -> Self {
        let defaults = DefaultTraceValues::get_global();
        Self {
            otlp: defaults.otlp.clone(),
            logs_otlp: defaults.logs_otlp.clone(),
            protocol: defaults.protocol,
            otlp_filter: defaults.otlp_filter.parse().expect("valid filter"),
            logs_otlp_filter: defaults.logs_otlp_filter.parse().expect("valid filter"),
            sample_ratio: defaults.sample_ratio,
            service_name: defaults.service_name.clone(),
            service_version: defaults.service_version.clone(),
        }
    }
}

fn parse_otlp_endpoint(arg: &str) -> eyre::Result<Url> {
    Url::parse(arg).wrap_err("Invalid URL for OTLP trace output")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_otlp_flags() {
        let args = TraceArgs::try_parse_from([
            "test",
            "--tracing-otlp=http://collector:4318/v1/traces",
            "--tracing-otlp-protocol",
            "http",
            "--tracing-otlp.filter=info",
            "--tracing-otlp.sample-ratio=0.25",
        ])
        .unwrap();
        assert_eq!(args.otlp.unwrap().as_str(), "http://collector:4318/v1/traces");
        assert_eq!(args.protocol, OtlpProtocol::Http);
        assert_eq!(args.sample_ratio, Some(0.25));
    }

    #[test]
    fn validates_http_and_grpc_endpoints() {
        let mut http = Url::parse("http://localhost:4318").unwrap();
        OtlpProtocol::Http.validate_endpoint(&mut http).unwrap();
        assert_eq!(http.path(), "/v1/traces");

        let mut grpc_bad = Url::parse("http://localhost:4317/v1/traces").unwrap();
        assert!(OtlpProtocol::Grpc.validate_endpoint(&mut grpc_bad).is_err());

        let mut grpc_ok = Url::parse("http://localhost:4317").unwrap();
        OtlpProtocol::Grpc.validate_endpoint(&mut grpc_ok).unwrap();
    }
}
