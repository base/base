#[cfg(feature = "telemetry")]
use crate::args::TelemetryArgs;
#[cfg(feature = "telemetry")]
use tracing_subscriber::{filter::Targets, Layer};
#[cfg(feature = "telemetry")]
use url::Url;

/// Setup telemetry layer with sampling and custom endpoint configuration
#[cfg(feature = "telemetry")]
pub fn setup_telemetry_layer(
    args: &TelemetryArgs,
) -> eyre::Result<impl Layer<tracing_subscriber::Registry>> {
    use tracing::level_filters::LevelFilter;

    if args.otlp_endpoint.is_none() {
        return Err(eyre::eyre!("OTLP endpoint is not set"));
    }

    if let Some(headers) = &args.otlp_headers {
        unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", headers) };
    }

    let otlp_layer = reth_tracing_otlp::span_layer(
        "op-rbuilder",
        &Url::parse(args.otlp_endpoint.as_ref().unwrap()).expect("Invalid OTLP endpoint"),
        reth_tracing_otlp::OtlpProtocol::Http,
    )?;

    let trace_filter = Targets::new()
        .with_default(LevelFilter::WARN)
        .with_target("op_rbuilder", LevelFilter::INFO)
        .with_target("payload_builder", LevelFilter::DEBUG);

    Ok(otlp_layer.with_filter(trace_filter))
}