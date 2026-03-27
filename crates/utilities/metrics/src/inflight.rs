//! RAII guard for tracking in-flight operations.

/// RAII guard that increments a gauge on creation and decrements it on drop.
///
/// Prefer the [`inflight!`] macro to construct this type.
#[cfg(feature = "metrics")]
pub struct InflightCounter {
    gauge: metrics::Gauge,
}

/// No-op guard used when the `metrics` feature is disabled.
#[cfg(not(feature = "metrics"))]
pub struct InflightCounter;

#[cfg(feature = "metrics")]
impl core::fmt::Debug for InflightCounter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InflightCounter").finish_non_exhaustive()
    }
}

#[cfg(not(feature = "metrics"))]
impl core::fmt::Debug for InflightCounter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InflightCounter").finish()
    }
}

#[cfg(feature = "metrics")]
impl InflightCounter {
    /// Creates a new guard that increments the gauge immediately.
    #[inline]
    pub fn new(gauge: metrics::Gauge) -> Self {
        gauge.increment(1);
        Self { gauge }
    }
}

#[cfg(not(feature = "metrics"))]
impl Default for InflightCounter {
    fn default() -> Self {
        Self
    }
}

#[cfg(not(feature = "metrics"))]
impl InflightCounter {
    /// Creates a no-op guard.
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(feature = "metrics")]
impl Drop for InflightCounter {
    fn drop(&mut self) {
        self.gauge.decrement(1);
    }
}

/// Creates an [`InflightCounter`] that tracks an in-flight operation.
///
/// # Examples
///
/// ```ignore
/// let _guard = base_metrics::inflight!(Metrics::in_flight_proofs());
/// // gauge decremented when _guard is dropped
/// ```
#[macro_export]
macro_rules! inflight {
    ($gauge:expr $(,)?) => {{
        #[cfg(feature = "metrics")]
        {
            $crate::InflightCounter::new($gauge)
        }
        #[cfg(not(feature = "metrics"))]
        {
            let _ = &$gauge;
            $crate::InflightCounter::new()
        }
    }};
}
