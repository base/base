use alloy_primitives::{Address, B256, Bytes};

/// A prover instance discovered from the infrastructure layer.
#[derive(Debug, Clone)]
pub struct ProverInstance {
    /// Unique identifier for the instance.
    ///
    /// K8s mode: pod DNS name (`{name}-{i}.{svc}.{ns}.svc.cluster.local:{port}`).
    /// AWS mode: EC2 instance ID (e.g. `i-0abc123def456`).
    pub instance_id: String,
    /// HTTP connection endpoint used to contact the instance.
    ///
    /// K8s mode: same as `instance_id` (pod DNS:port).
    /// AWS mode: private IP and port (e.g. `10.0.1.5:8000`).
    pub endpoint: String,
    /// Current health status of the instance.
    pub health_status: InstanceHealthStatus,
}

/// Health status of a discovered prover instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceHealthStatus {
    /// ALB health checks are in progress (AWS mode only — instance just started).
    Initial,
    /// Instance is reachable and passing health checks.
    Healthy,
    /// Instance did not respond to the poll or is failing health checks.
    Unhealthy,
    /// ALB is draining connections from this instance (AWS mode only).
    Draining,
}

impl InstanceHealthStatus {
    /// Returns `true` if the instance should be registered on-chain.
    ///
    /// Both `Initial` (AWS warm-up) and `Healthy` instances are candidates for
    /// registration. `Unhealthy` and `Draining` instances are not.
    pub const fn should_register(&self) -> bool {
        matches!(self, Self::Initial | Self::Healthy)
    }

    /// Maps an AWS ELB target health state string to [`InstanceHealthStatus`].
    ///
    /// Used by `AwsTargetGroupDiscovery` to convert `describe_target_health` responses.
    pub fn from_aws_state(state: &str) -> Self {
        match state {
            "initial" => Self::Initial,
            "healthy" => Self::Healthy,
            "draining" => Self::Draining,
            _ => Self::Unhealthy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_should_register() {
        assert!(InstanceHealthStatus::Healthy.should_register());
    }

    #[test]
    fn initial_should_register() {
        assert!(InstanceHealthStatus::Initial.should_register());
    }

    #[test]
    fn unhealthy_should_not_register() {
        assert!(!InstanceHealthStatus::Unhealthy.should_register());
    }

    #[test]
    fn draining_should_not_register() {
        assert!(!InstanceHealthStatus::Draining.should_register());
    }

    #[test]
    fn from_aws_state_initial() {
        assert_eq!(InstanceHealthStatus::from_aws_state("initial"), InstanceHealthStatus::Initial);
    }

    #[test]
    fn from_aws_state_healthy() {
        assert_eq!(InstanceHealthStatus::from_aws_state("healthy"), InstanceHealthStatus::Healthy);
    }

    #[test]
    fn from_aws_state_draining() {
        assert_eq!(
            InstanceHealthStatus::from_aws_state("draining"),
            InstanceHealthStatus::Draining
        );
    }

    #[test]
    fn from_aws_state_unhealthy() {
        assert_eq!(
            InstanceHealthStatus::from_aws_state("unhealthy"),
            InstanceHealthStatus::Unhealthy
        );
    }

    #[test]
    fn from_aws_state_unknown_maps_to_unhealthy() {
        assert_eq!(
            InstanceHealthStatus::from_aws_state("unavailable"),
            InstanceHealthStatus::Unhealthy
        );
    }
}

/// Response from polling a prover's `enclave_signerAttestation` endpoint.
#[derive(Debug, Clone)]
pub struct AttestationResponse {
    /// Raw Nitro attestation document (`COSE_Sign1` bytes).
    pub attestation_bytes: Bytes,
    /// Ethereum address derived from the attestation's embedded public key.
    pub signer_address: Address,
}

/// ZK proof ready for on-chain registration via `TEEProverRegistry.registerSigner`.
#[derive(Debug, Clone)]
pub struct AttestationProof {
    /// ABI-encoded `VerifierJournal` output from the Boundless / Automata SDK.
    pub output: Bytes,
    /// ZK proof bytes for submission alongside `output`.
    pub proof_bytes: Bytes,
}

/// A signer currently registered on-chain via `TEEProverRegistry`.
#[derive(Debug, Clone)]
pub struct RegisteredSigner {
    /// The signer's Ethereum address.
    pub address: Address,
    /// The `keccak256(PCR0)` measurement hash the signer was registered under.
    pub pcr0: B256,
}
