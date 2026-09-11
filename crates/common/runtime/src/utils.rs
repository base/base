//! Task utility functions.

pub use thread_priority::{self, *};

/// Runs the given closure exactly once per call site.
///
/// Each invocation expands to its own `static Once`, so two `once!` calls in the same function
/// are independent. Useful for one-time thread setup (priority, affinity) on threads that may
/// be re-entered (e.g. tokio blocking pool).
#[macro_export]
macro_rules! once {
    ($e:expr) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once($e);
    }};
}

/// Increases the current thread's priority.
///
/// Tries [`ThreadPriority::Max`] first. If that fails (e.g. missing `CAP_SYS_NICE`),
/// falls back to a moderate bump via [`ThreadPriority::Crossplatform`] (~5 nice points
/// on unix). Failures are logged at `debug` level.
pub fn increase_thread_priority() {
    let thread_name = std::thread::current().name().unwrap_or("unnamed").to_string();
    if let Err(err) = ThreadPriority::Max.set_for_current() {
        tracing::debug!(%thread_name, ?err, "failed to set max thread priority, trying moderate bump; grant CAP_SYS_NICE to the process to enable this");
        // Crossplatform value 62/99 ≈ nice -5 on unix.
        let fallback = ThreadPriority::Crossplatform(
            ThreadPriorityValue::try_from(62u8).expect("62 is within the valid 0..100 range"),
        );
        if let Err(err) = fallback.set_for_current() {
            tracing::debug!(%thread_name, ?err, "failed to set moderate thread priority");
        }
    }
}
