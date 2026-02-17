//! Contains the [`HealthStatus`] enum, which represents the health status of a backend in Roxy.

use std::time::{Duration, Instant};

/// Health status of a backend.
#[derive(Debug, Clone, Copy)]
pub enum HealthStatus {
    /// Backend is healthy.
    Healthy,
    /// Backend is degraded with high latency.
    Degraded {
        /// Current latency EMA.
        latency_ema: Duration,
    },
    /// Backend is unhealthy with high error rate.
    Unhealthy {
        /// Current error rate (0.0 to 1.0).
        error_rate: f64,
    },
    /// Backend is temporarily banned.
    Banned {
        /// Time until the ban expires.
        until: Instant,
    },
}
