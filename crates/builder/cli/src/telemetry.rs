//! Telemetry CLI arguments.

use clap::Args;
use reth_tracing_otlp::OtlpConfig;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{Layer, filter::Targets};
use url::Url;

/// Parameters for telemetry configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub struct TelemetryArgs {
    /// `OpenTelemetry` endpoint for traces
    #[arg(long = "telemetry.otlp-endpoint", env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,

    /// `OpenTelemetry` headers for authentication
    #[arg(long = "telemetry.otlp-headers", env = "OTEL_EXPORTER_OTLP_HEADERS")]
    pub otlp_headers: Option<String>,

    /// Inverted sampling frequency in blocks. 1 - each block, 100 - every 100th block.
    #[arg(long = "telemetry.sampling-ratio", env = "SAMPLING_RATIO", default_value = "100")]
    pub sampling_ratio: u64,
}

impl TelemetryArgs {
    /// Setup telemetry layer with sampling and custom endpoint configuration
    pub fn setup(&self) -> eyre::Result<impl Layer<tracing_subscriber::Registry>> {
        if self.otlp_endpoint.is_none() {
            return Err(eyre::eyre!("OTLP endpoint is not set"));
        }

        // Otlp uses env vars inside
        if let Some(headers) = &self.otlp_headers {
            // SAFETY: This is called early during initialization before spawning async tasks.
            // The OTLP library reads this environment variable during setup.
            unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", headers) };
        }

        // Create OTLP layer with custom configuration
        let otlp_config = OtlpConfig::new(
            "base-builder",
            Url::parse(self.otlp_endpoint.as_ref().unwrap()).expect("Invalid OTLP endpoint"),
            reth_tracing_otlp::OtlpProtocol::Http,
            Some((self.sampling_ratio as f64) / 100.0),
        )?;
        let otlp_layer = reth_tracing_otlp::span_layer(otlp_config)?;

        // Create a trace filter that sends more data to OTLP but less to stdout
        let trace_filter = Targets::new()
            .with_default(LevelFilter::WARN)
            .with_target("base_builder", LevelFilter::INFO)
            .with_target("payload_builder", LevelFilter::DEBUG);

        let filtered_layer = otlp_layer.with_filter(trace_filter);

        Ok(filtered_layer)
    }
}
