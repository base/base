//! Prewarm configuration.

/// Configuration for predicate-state prewarming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrewarmConfig {
    /// Whether prewarming runs at all. Disabled by default.
    pub enabled: bool,
    /// Number of IO worker threads in the shared pool. Each worker owns its own
    /// [`CachedStateProvider`] and cache handle for the duration of one job.
    pub worker_count: usize,
    /// Bounded lookahead: transactions scanned ahead of the build loop (the initial
    /// scheduling burst, with one further advance per consumed candidate).
    pub lookahead: usize,
    /// Maximum distinct state keys scheduled per build; once reached the scheduler
    /// saturates and stops advancing the lookahead cursor.
    pub key_cap: usize,
    /// Whether workers additionally run full transaction simulation to warm each
    /// simulated transaction's entire EVM read set (not just declared predicate
    /// keys). Strict opt-in on top of `enabled`; disabled by default.
    pub simulate: bool,
    /// Maximum simulations outstanding (queued or in flight) at any time. Simulation is
    /// far heavier than a single key read, so this bounds how far warming runs ahead of
    /// the build loop and self-throttles to worker throughput. Only meaningful when
    /// `simulate` is set.
    pub sim_lookahead: usize,
}

impl Default for PrewarmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            worker_count: 2,
            lookahead: 64,
            key_cap: 4096,
            simulate: false,
            sim_lookahead: 16,
        }
    }
}
