//! Timing helper macros built on top of the `base-metrics` timer types.

/// Creates a [`DropTimer`] (or [`NoopDropTimer`]) that records elapsed duration
/// to a histogram metric on drop.
///
/// # Examples
///
/// ```ignore
/// let _timer = base_metrics::timed!(Metrics::proof_duration_seconds());
///
/// let mut timer = base_metrics::timed!(Metrics::witness_build_duration_seconds());
/// timer.stop();
/// ```
#[macro_export]
macro_rules! timed {
    ($metric_handle:expr) => {{
        #[cfg(feature = "metrics")]
        {
            $crate::DropTimer::new($metric_handle)
        }
        #[cfg(not(feature = "metrics"))]
        {
            let _ = &$metric_handle;
            $crate::NoopDropTimer
        }
    }};
}

/// Executes a block and records its duration to a histogram metric.
///
/// Returns the value of the block expression.
///
/// # Examples
///
/// ```ignore
/// let result = base_metrics::time!(Metrics::request_duration(), {
///     do_work().await
/// });
/// ```
#[macro_export]
macro_rules! time {
    ($metric_handle:expr, $body:block) => {{
        let mut __timer = $crate::timed!($metric_handle);
        let __result = $body;
        __timer.stop();
        __result
    }};
}
