use alloy_primitives::{Address, B256};
use url::Url;

/// A prover instance discovered from the infrastructure layer.
#[derive(Debug, Clone)]
pub struct ProverInstance {
    /// EC2 instance ID (e.g. `i-0abc123def456`).
    pub instance_id: String,
    /// HTTP endpoint URL for the prover (e.g. `http://10.0.1.5:8000/`).
    pub endpoint: Url,
    /// Current health status of the instance.
    pub health_status: InstanceHealthStatus,
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

/// A signer currently registered on-chain via `TEEProverRegistry`.
#[derive(Debug, Clone)]
pub struct RegisteredSigner {
    /// The signer's Ethereum address.
    pub address: Address,
    /// The `keccak256(PCR0)` measurement hash the signer was registered under.
    pub pcr0: B256,
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::initial(InstanceHealthStatus::Initial, true)]
    #[case::healthy(InstanceHealthStatus::Healthy, true)]
    #[case::unhealthy(InstanceHealthStatus::Unhealthy, false)]
    #[case::draining(InstanceHealthStatus::Draining, false)]
    fn should_register(#[case] status: InstanceHealthStatus, #[case] expected: bool) {
        assert_eq!(status.should_register(), expected);
    }

    #[rstest]
    #[case::initial("initial", InstanceHealthStatus::Initial)]
    #[case::healthy("healthy", InstanceHealthStatus::Healthy)]
    #[case::draining("draining", InstanceHealthStatus::Draining)]
    #[case::unhealthy("unhealthy", InstanceHealthStatus::Unhealthy)]
    #[case::unavailable("unavailable", InstanceHealthStatus::Unhealthy)]
    #[case::empty("", InstanceHealthStatus::Unhealthy)]
    #[case::bogus("bogus", InstanceHealthStatus::Unhealthy)]
    fn from_aws_state(#[case] input: &str, #[case] expected: InstanceHealthStatus) {
        assert_eq!(InstanceHealthStatus::from_aws_state(input), expected);
    }
}
