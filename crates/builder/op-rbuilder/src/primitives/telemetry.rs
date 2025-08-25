use crate::args::TelemetryArgs;
use tracing_subscriber::{Layer, filter::Targets};

/// Setup telemetry layer with sampling and custom endpoint configuration
pub fn setup_telemetry_layer(
    args: &TelemetryArgs,
) -> eyre::Result<impl Layer<tracing_subscriber::Registry>> {
    use tracing::level_filters::LevelFilter;

    // Otlp uses evn vars inside
    if let Some(endpoint) = &args.otlp_endpoint {
        unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint) };
    }
    if let Some(headers) = &args.otlp_headers {
        unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", headers) };
    }

    // Create OTLP layer with custom configuration
    let otlp_layer = reth_tracing_otlp::layer("op-rbuilder");

    // Create a trace filter that sends more data to OTLP but less to stdout
    let trace_filter = Targets::new()
        .with_default(LevelFilter::WARN)
        .with_target("op_rbuilder", LevelFilter::INFO)
        .with_target("payload_builder", LevelFilter::DEBUG);

    let filtered_layer = otlp_layer.with_filter(trace_filter);

    Ok(filtered_layer)
}
