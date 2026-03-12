use tracing::Level;

/// Guard for the tracing subscriber lifecycle.
#[derive(Debug)]
pub struct TracingGuard {
    _guard: (),
}

/// Initializes the tracing subscriber.
pub fn init_tracing() -> TracingGuard {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    TracingGuard { _guard: () }
}
