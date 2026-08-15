//! Tracing subscriber initialization for CLI applications.

use std::{fmt, io, sync::Once};

use tracing::Subscriber;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    EnvFilter, Layer,
    fmt::{
        FmtContext, FormattedFields,
        format::{FormatEvent, FormatFields, Writer},
        time::{FormatTime, SystemTime},
    },
    layer::SubscriberExt,
    registry::{LookupSpan, Registry},
    util::SubscriberInitExt,
};

use crate::{FileLogConfig, LogConfig, LogFormat, LogRotation, StdoutLogConfig};

/// Custom logfmt formatter for tracing events.
///
/// Outputs logs in logfmt format: `ts=... level=info target=myapp msg="hello world"`
#[derive(Debug, Clone, Copy, Default)]
pub struct LogfmtFormatter;

impl<S, N> FormatEvent<S, N> for LogfmtFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();

        // Write timestamp
        let time_format = SystemTime;
        write!(writer, "time=\"")?;
        time_format.format_time(&mut writer)?;
        write!(writer, "\" ")?;

        // Write level
        write!(writer, "level={} ", meta.level())?;

        // Write target
        write!(writer, "target={} ", meta.target())?;

        // Write the message and fields
        write!(writer, "msg=\"")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        write!(writer, "\"")?;

        // Write span context
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                write!(writer, " {}={{", span.name())?;
                if let Some(fields) = span.extensions().get::<FormattedFields<N>>() {
                    write!(writer, "{fields}")?;
                }
                write!(writer, "}}")?;
            }
        }

        writeln!(writer)
    }
}

impl LogConfig {
    /// Initialize the tracing subscriber with the configured options.
    ///
    /// This sets the global default subscriber. Should only be called once.
    ///
    /// Includes default noise suppression for overly verbose crates (e.g., `discv5=error`).
    pub fn init_tracing_subscriber(&self) -> eyre::Result<()> {
        self.init_tracing_subscriber_with_directives(&[])
    }

    /// Initialize the tracing subscriber with additional application-specific noise-suppression
    /// directives appended on top of the workspace defaults (e.g., `discv5=error`).
    ///
    /// This sets the global default subscriber. Should only be called once.
    pub fn init_tracing_subscriber_with_directives(&self, directives: &[&str]) -> eyre::Result<()> {
        self.init_tracing_subscriber_with_filter(self.build_filter_from_directives(directives))
    }

    /// Initialize the tracing subscriber with additional application-specific directives and an
    /// optional extra layer.
    ///
    /// The extra layer can be used to attach additional subscribers such as OTLP exporting.
    pub fn init_tracing_subscriber_with_directives_and_extra_layer(
        &self,
        directives: &[&str],
        extra_layer: Option<Box<dyn Layer<Registry> + Send + Sync>>,
    ) -> eyre::Result<()> {
        self.init_tracing_subscriber_with_filter_and_extra_layer(
            self.build_filter_from_directives(directives),
            extra_layer,
        )
    }

    /// Initialize the tracing subscriber with a custom filter.
    ///
    /// This sets the global default subscriber. Should only be called once.
    pub fn init_tracing_subscriber_with_filter(&self, filter: EnvFilter) -> eyre::Result<()> {
        self.init_tracing_subscriber_with_filter_and_extra_layer(filter, None)
    }

    /// Initialize the tracing subscriber with [`reth_node_core::args::TraceArgs`] OTLP export.
    ///
    /// Registers the W3C `TraceContextPropagator`, builds an optional OTLP span layer from
    /// `trace_args`, and initializes the subscriber with the given noise-suppression directives.
    /// This is the preferred entry point for binaries that want OTLP support.
    pub fn init_with_trace_args(
        &self,
        trace_args: &reth_node_core::args::TraceArgs,
        directives: &[&str],
    ) -> eyre::Result<()> {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );

        let otlp_layer: Option<Box<dyn Layer<Registry> + Send + Sync>> = {
            #[cfg(feature = "otlp")]
            if let Some(mut endpoint) = trace_args.otlp.clone() {
                trace_args.protocol.validate_endpoint(&mut endpoint)?;
                let config = reth_tracing_otlp::OtlpConfig::new(
                    trace_args.service_name.clone(),
                    endpoint,
                    trace_args.protocol,
                    trace_args.sample_ratio,
                )?;
                let layer = reth_tracing_otlp::span_layer(config)?
                    .with_filter(trace_args.otlp_filter.clone());
                Some(Box::new(layer))
            } else {
                None
            }
            #[cfg(not(feature = "otlp"))]
            {
                let _ = trace_args;
                None
            }
        };

        self.init_tracing_subscriber_with_directives_and_extra_layer(directives, otlp_layer)
    }

    fn build_filter_from_directives(&self, directives: &[&str]) -> EnvFilter {
        let mut filter = EnvFilter::builder()
            .with_default_directive(self.global_level.into())
            .from_env_lossy()
            // Suppress noisy discovery logs by default
            .add_directive("discv5=error".parse().expect("valid directive"));

        for directive in directives {
            filter = filter.add_directive(directive.parse().expect("valid directive"));
        }

        filter
    }

    fn init_tracing_subscriber_with_filter_and_extra_layer(
        &self,
        filter: EnvFilter,
        extra_layer: Option<Box<dyn Layer<Registry> + Send + Sync>>,
    ) -> eyre::Result<()> {
        let registry = tracing_subscriber::registry().with(extra_layer);

        // Build stdout layer
        let stdout_layer =
            self.stdout_logs.as_ref().map(|config| build_stdout_layer(config, filter.clone()));

        // Build file layer
        let file_layer =
            self.file_logs.as_ref().map(|config| build_file_layer(config, filter.clone()));

        // Combine and init
        registry
            .with(stdout_layer)
            .with(file_layer)
            .try_init()
            .map_err(|e| eyre::eyre!("Failed to initialize tracing subscriber: {}", e))
    }
}

/// Build a stdout layer with the specified format.
fn build_stdout_layer<S>(
    config: &StdoutLogConfig,
    filter: EnvFilter,
) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    let base = tracing_subscriber::fmt::layer()
        .with_writer(io::stdout)
        .with_ansi(true)
        .with_timer(SystemTime);

    match config.format {
        LogFormat::Full => Box::new(base.with_filter(filter)),
        LogFormat::Compact => Box::new(base.compact().with_filter(filter)),
        LogFormat::Json => Box::new(base.json().with_filter(filter)),
        LogFormat::Pretty => Box::new(base.pretty().with_filter(filter)),
        LogFormat::Logfmt => Box::new(base.event_format(LogfmtFormatter).with_filter(filter)),
    }
}

/// Build a file layer with the specified format and rotation.
fn build_file_layer<S>(config: &FileLogConfig, filter: EnvFilter) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync,
{
    let rotation = match config.rotation {
        LogRotation::Minutely => Rotation::MINUTELY,
        LogRotation::Hourly => Rotation::HOURLY,
        LogRotation::Daily => Rotation::DAILY,
        LogRotation::Never => Rotation::NEVER,
    };

    let appender = RollingFileAppender::new(rotation, &config.directory_path, "base-node.log");

    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    // Store guard in a static to prevent dropping (leak intentionally to keep writer alive)
    std::mem::forget(guard);

    let base = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_timer(SystemTime);

    match config.format {
        LogFormat::Full => Box::new(base.with_filter(filter)),
        LogFormat::Compact => Box::new(base.compact().with_filter(filter)),
        LogFormat::Json => Box::new(base.json().with_filter(filter)),
        LogFormat::Pretty => Box::new(base.pretty().with_filter(filter)),
        LogFormat::Logfmt => Box::new(base.event_format(LogfmtFormatter).with_filter(filter)),
    }
}

/// Initialize tracing for tests with sensible defaults.
///
/// Uses `tracing_subscriber::fmt().with_test_writer()` for test output capture.
/// Only initializes once (safe to call from multiple tests).
pub fn init_test_tracing() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let filter = EnvFilter::builder()
            .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
            .from_env_lossy();

        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_test_writer().try_init();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_test_tracing_idempotent() {
        init_test_tracing();
        init_test_tracing(); // Should not panic
    }
}
