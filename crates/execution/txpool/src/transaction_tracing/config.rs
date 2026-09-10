//! Node service runtime settings.

/// Transaction pool tracing configuration.
#[derive(Debug, Clone)]
pub struct TxpoolConfig {
    /// Enables transaction tracing.
    pub tracing_enabled: bool,
    /// Emits `info`-level logs for transaction tracing when enabled.
    pub tracing_logs_enabled: bool,
    /// Optional node role label attached to durable transaction events.
    pub transaction_event_node_role: Option<String>,
}
