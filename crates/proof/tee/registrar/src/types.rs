use std::time::SystemTime;

pub use base_proof_tee_attestation::TeeAttestationKind as AttestationKind;
use url::Url;

/// A prover instance discovered from the infrastructure layer.
#[derive(Debug, Clone)]
pub struct ProverInstance {
    /// Infrastructure instance ID (e.g. `i-0abc123def456` or a GCE instance name).
    pub instance_id: String,
    /// HTTP endpoint URL for the prover (e.g. `http://10.0.1.5:8000/`).
    pub endpoint: Url,
    /// Attestation platform exposed by this prover.
    pub attestation_kind: AttestationKind,
    /// Current health status of the instance.
    pub health_status: InstanceHealthStatus,
    /// Launch time of the instance. Used to determine if recently-launched
    /// unhealthy instances should still be eligible for registration.
    pub launch_time: Option<SystemTime>,
}

/// Health status of a discovered prover instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceHealthStatus {
    /// ALB health checks are in progress — instance just started.
    Initial,
    /// Instance is reachable and passing health checks.
    Healthy,
    /// Instance did not respond to the poll or is failing health checks.
    Unhealthy,
    /// ALB is draining connections from this instance.
    Draining,
}
