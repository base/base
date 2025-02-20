use tracing_subscriber::{fmt, EnvFilter};

pub fn setup_logging() {
    let format = fmt::format()
        .with_level(true)
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(true);

    // Initialize logging using RUST_LOG environment variable, defaulting to INFO level
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| {
            EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into())
        }))
        .event_format(format)
        .init();
}
