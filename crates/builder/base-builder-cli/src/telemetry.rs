//! Telemetry CLI arguments.

use clap::Args;

/// Parameters for telemetry configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub struct TelemetryArgs {
    /// OpenTelemetry endpoint for traces
    #[arg(long = "telemetry.otlp-endpoint", env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,

    /// OpenTelemetry headers for authentication
    #[arg(long = "telemetry.otlp-headers", env = "OTEL_EXPORTER_OTLP_HEADERS")]
    pub otlp_headers: Option<String>,

    /// Inverted sampling frequency in blocks. 1 - each block, 100 - every 100th block.
    #[arg(long = "telemetry.sampling-ratio", env = "SAMPLING_RATIO", default_value = "100")]
    pub sampling_ratio: u64,
}
