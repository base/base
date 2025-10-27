use tracing::warn;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_logger(log_level: &str) {
    let level = match log_level.to_lowercase().as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "info" => tracing::Level::INFO,
        "warn" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => {
            warn!("Invalid log level '{}', defaulting to 'info'", log_level);
            tracing::Level::INFO
        }
    };

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level.to_string())),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}
