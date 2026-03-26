//! No-op metric types used when the `metrics` feature is disabled.

/// A no-op metric handle that compiles to nothing, providing zero-cost stubs
/// when the `metrics` feature is disabled.
#[derive(Debug, Clone, Copy)]
pub struct NoopMetric;

impl NoopMetric {
    /// Gauge/counter set — compiles away.
    #[inline(always)]
    pub fn set<T>(&self, _: T) {}
    /// Counter increment — compiles away.
    #[inline(always)]
    pub fn increment<T>(&self, _: T) {}
    /// Counter absolute — compiles away.
    #[inline(always)]
    pub fn absolute<T>(&self, _: T) {}
    /// Histogram record — compiles away.
    #[inline(always)]
    pub fn record<T>(&self, _: T) {}
    /// Gauge decrement — compiles away.
    #[inline(always)]
    pub fn decrement<T>(&self, _: T) {}
}

/// No-op drop timer used when the `metrics` feature is disabled.
#[derive(Debug, Clone, Copy)]
pub struct NoopDropTimer;

impl NoopDropTimer {
    /// Stop — compiles away.
    #[inline(always)]
    pub const fn stop(&mut self) {}
}
