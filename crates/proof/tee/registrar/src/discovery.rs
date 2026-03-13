//! AWS ALB target group instance discovery.

use std::{collections::HashMap, net::IpAddr};

use async_trait::async_trait;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_elasticloadbalancingv2::Client as ElbClient;
use tracing::{debug, warn};

use crate::{InstanceDiscovery, InstanceHealthStatus, ProverInstance, RegistrarError, Result};

/// Maps an AWS ALB target health state string to [`InstanceHealthStatus`].
///
/// Unknown or unrecognised states are treated as [`InstanceHealthStatus::Unhealthy`]
/// to avoid routing work to targets whose status cannot be determined.
pub fn health_state_from_str(state: &str) -> InstanceHealthStatus {
    match state {
        "initial" => InstanceHealthStatus::Initial,
        "healthy" => InstanceHealthStatus::Healthy,
        "draining" => InstanceHealthStatus::Draining,
        _ => InstanceHealthStatus::Unhealthy,
    }
}

/// Discovers prover instances by querying an AWS ALB target group.
///
/// Uses `describe_target_health` to enumerate all registered targets, including
/// those in the `Initial` state that have not yet passed the ALB health check.
/// This allows the registrar to detect and pre-register new instances during the
/// ALB warm-up window (typically ~1 hour) before they begin receiving traffic.
///
/// Private IP addresses are resolved from the EC2 instance IDs returned by the
/// target group via a `describe_instances` call.
#[derive(Debug)]
pub struct AwsTargetGroupDiscovery {
    elb_client: ElbClient,
    ec2_client: Ec2Client,
    target_group_arn: String,
}

impl AwsTargetGroupDiscovery {
    /// Creates a new discovery client for the given target group ARN and AWS region.
    pub async fn new(target_group_arn: String, aws_region: String) -> Result<Self> {
        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_ec2::config::Region::new(aws_region))
            .load()
            .await;
        let elb_client = ElbClient::new(&sdk_config);
        let ec2_client = Ec2Client::new(&sdk_config);
        Ok(Self { elb_client, ec2_client, target_group_arn })
    }

    /// Builds a [`ProverInstance`] list from a health map and raw `(instance_id, private_ip_str)`
    /// pairs that have already been extracted from EC2 reservations.
    ///
    /// Extracted from [`Self::discover_instances`] so that the IP-parse, health-map lookup,
    /// and assembly logic can be unit-tested without constructing AWS SDK mock clients.
    pub fn assemble_prover_instances(
        health_map: &HashMap<String, InstanceHealthStatus>,
        pairs: impl IntoIterator<Item = (String, String)>,
    ) -> Vec<ProverInstance> {
        let mut instances = Vec::new();
        for (instance_id, private_ip_str) in pairs {
            let private_ip: IpAddr = match private_ip_str.parse() {
                Ok(ip) => ip,
                Err(e) => {
                    warn!(instance_id = %instance_id, error = %e, "invalid private IP, skipping");
                    continue;
                }
            };
            let health_status =
                health_map.get(&instance_id).cloned().unwrap_or(InstanceHealthStatus::Unhealthy);

            debug!(
                instance_id = %instance_id,
                private_ip = %private_ip,
                health = ?health_status,
                "discovered prover instance"
            );

            instances.push(ProverInstance { instance_id, private_ip, health_status });
        }
        instances
    }
}

#[async_trait]
impl InstanceDiscovery for AwsTargetGroupDiscovery {
    async fn discover_instances(&self) -> Result<Vec<ProverInstance>> {
        // Step 1: Query the target group for all registered targets and their health status.
        // This includes instances in the Initial state before the ALB routes traffic to them.
        let health_output = self
            .elb_client
            .describe_target_health()
            .target_group_arn(&self.target_group_arn)
            .send()
            .await
            .map_err(|e| RegistrarError::Discovery(Box::new(e)))?;

        let target_descriptions = health_output.target_health_descriptions();
        if target_descriptions.is_empty() {
            debug!(target_group = %self.target_group_arn, "no targets in target group");
            return Ok(vec![]);
        }

        // Step 2: Build an instance_id → health_status map and collect instance IDs for EC2 lookup.
        let mut health_map: HashMap<String, InstanceHealthStatus> = HashMap::new();
        let mut instance_ids: Vec<String> = Vec::new();

        for desc in target_descriptions {
            let Some(instance_id) = desc.target().and_then(|t| t.id()) else {
                warn!("target group entry missing instance ID, skipping");
                continue;
            };
            let health_status = desc
                .target_health()
                .and_then(|h| h.state())
                .map(|s| health_state_from_str(s.as_str()))
                .unwrap_or(InstanceHealthStatus::Unhealthy);

            health_map.insert(instance_id.to_string(), health_status);
            instance_ids.push(instance_id.to_string());
        }

        if instance_ids.is_empty() {
            return Ok(vec![]);
        }

        // Step 3: Resolve private IPs for all instance IDs in a single EC2 call.
        let instances_output = self
            .ec2_client
            .describe_instances()
            .set_instance_ids(Some(instance_ids))
            .send()
            .await
            .map_err(|e| RegistrarError::Discovery(Box::new(e)))?;

        // Step 4: Extract (instance_id, private_ip_str) pairs, skipping incomplete entries.
        let mut pairs: Vec<(String, String)> = Vec::new();
        for reservation in instances_output.reservations() {
            for instance in reservation.instances() {
                let Some(instance_id) = instance.instance_id() else {
                    warn!("EC2 instance returned with no ID, skipping");
                    continue;
                };
                let Some(private_ip_str) = instance.private_ip_address() else {
                    warn!(instance_id = %instance_id, "EC2 instance has no private IP, skipping");
                    continue;
                };
                pairs.push((instance_id.to_string(), private_ip_str.to_string()));
            }
        }

        Ok(Self::assemble_prover_instances(&health_map, pairs))
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    #[test]
    fn health_state_initial() {
        assert_eq!(health_state_from_str("initial"), InstanceHealthStatus::Initial);
    }

    #[test]
    fn health_state_healthy() {
        assert_eq!(health_state_from_str("healthy"), InstanceHealthStatus::Healthy);
    }

    #[test]
    fn health_state_draining() {
        assert_eq!(health_state_from_str("draining"), InstanceHealthStatus::Draining);
    }

    #[test]
    fn health_state_unhealthy() {
        assert_eq!(health_state_from_str("unhealthy"), InstanceHealthStatus::Unhealthy);
    }

    #[test]
    fn health_state_unknown_maps_to_unhealthy() {
        assert_eq!(health_state_from_str("unavailable"), InstanceHealthStatus::Unhealthy);
        assert_eq!(health_state_from_str(""), InstanceHealthStatus::Unhealthy);
        assert_eq!(health_state_from_str("bogus"), InstanceHealthStatus::Unhealthy);
    }

    #[test]
    fn assemble_empty_pairs_returns_empty() {
        let health_map = HashMap::new();
        assert!(AwsTargetGroupDiscovery::assemble_prover_instances(&health_map, vec![]).is_empty());
    }

    #[test]
    fn assemble_invalid_ip_is_skipped() {
        let health_map = HashMap::new();
        let pairs = vec![("i-123".to_string(), "not-an-ip".to_string())];
        assert!(AwsTargetGroupDiscovery::assemble_prover_instances(&health_map, pairs).is_empty());
    }

    #[test]
    fn assemble_instance_not_in_health_map_defaults_to_unhealthy() {
        let health_map = HashMap::new();
        let pairs = vec![("i-123".to_string(), "10.0.0.1".to_string())];
        let result = AwsTargetGroupDiscovery::assemble_prover_instances(&health_map, pairs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].instance_id, "i-123");
        assert_eq!(result[0].health_status, InstanceHealthStatus::Unhealthy);
    }

    #[test]
    fn assemble_health_status_is_looked_up_from_map() {
        let mut health_map = HashMap::new();
        health_map.insert("i-abc".to_string(), InstanceHealthStatus::Healthy);
        health_map.insert("i-def".to_string(), InstanceHealthStatus::Initial);
        let pairs = vec![
            ("i-abc".to_string(), "10.0.0.1".to_string()),
            ("i-def".to_string(), "10.0.0.2".to_string()),
        ];
        let result = AwsTargetGroupDiscovery::assemble_prover_instances(&health_map, pairs);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].health_status, InstanceHealthStatus::Healthy);
        assert_eq!(result[1].health_status, InstanceHealthStatus::Initial);
    }

    #[test]
    fn assemble_parses_ipv4_address() {
        let health_map = HashMap::new();
        let pairs = vec![("i-123".to_string(), "192.168.1.100".to_string())];
        let result = AwsTargetGroupDiscovery::assemble_prover_instances(&health_map, pairs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].private_ip, "192.168.1.100".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn assemble_invalid_ip_does_not_affect_valid_pairs() {
        let health_map = HashMap::new();
        let pairs = vec![
            ("i-bad".to_string(), "not-an-ip".to_string()),
            ("i-good".to_string(), "10.0.0.5".to_string()),
        ];
        let result = AwsTargetGroupDiscovery::assemble_prover_instances(&health_map, pairs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].instance_id, "i-good");
    }
}
