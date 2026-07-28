use url::Url;

/// A prover instance discovered from the infrastructure layer.
#[derive(Debug, Clone)]
pub struct ProverInstance {
    /// EC2 instance ID (e.g. `i-0abc123def456`).
    pub instance_id: String,
    /// HTTP endpoint URL for the prover (e.g. `http://10.0.1.5:8000/`).
    pub endpoint: Url,
    /// Current health status from discovery or direct readiness probing.
    pub health_status: InstanceHealthStatus,
}

/// Health status of a discovered prover instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceHealthStatus {
    /// ALB health checks are in progress — instance just started.
    Initial,
    /// Instance is reachable and passing readiness checks.
    Healthy,
    /// Instance did not respond to `readyz` or is failing health checks.
    Unhealthy,
    /// ALB is draining connections from this instance.
    Draining,
}
