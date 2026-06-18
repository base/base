//! AWS ALB target group instance discovery.

use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_elasticloadbalancingv2::Client as ElbClient;
use tracing::{debug, warn};
use url::Url;

use crate::{InstanceDiscovery, InstanceHealthStatus, ProverInstance, RegistrarError, Result};

/// Discovers prover instances via AWS Elastic Load Balancing target groups.
///
/// Queries `describe_target_health` to enumerate registered targets, then
/// resolves each EC2 instance's private IP address via `describe_instances`.
/// Health state is mapped from the ALB target health state, supporting the
/// `Initial` warm-up window during which new instances should be registered.
#[derive(Debug)]
pub struct AwsTargetGroupDiscovery {
    elb_client: ElbClient,
    ec2_client: Ec2Client,
    target_group_arn: String,
    port: u16,
}

impl AwsTargetGroupDiscovery {
    /// Creates a new `AwsTargetGroupDiscovery` with the given AWS config.
    pub fn new(aws_config: &aws_config::SdkConfig, target_group_arn: String, port: u16) -> Self {
        let elb_client = ElbClient::new(aws_config);
        let ec2_client = Ec2Client::new(aws_config);
        Self { elb_client, ec2_client, target_group_arn, port }
    }
}

impl InstanceDiscovery for AwsTargetGroupDiscovery {
    async fn discover_instances(&self) -> Result<Vec<ProverInstance>> {
        let elb_resp = self
            .elb_client
            .describe_target_health()
            .target_group_arn(&self.target_group_arn)
            .send()
            .await
            .map_err(|e| RegistrarError::Discovery(Box::new(e)))?;

        let mut health_map: HashMap<String, InstanceHealthStatus> = HashMap::new();
        for desc in elb_resp.target_health_descriptions() {
            let Some(instance_id) = desc.target().and_then(|t| t.id()) else {
                warn!("target group entry missing instance ID, skipping");
                continue;
            };
            if !instance_id.starts_with("i-") {
                warn!(
                    id = %instance_id,
                    "target is not an instance-type target (id does not start with \
                     'i-'); is the target group type set to 'instance'? skipping"
                );
                continue;
            }
            let health_status = desc
                .target_health()
                .and_then(|h| h.state())
                .map(|s| InstanceHealthStatus::from_aws_state(s.as_str()))
                .unwrap_or(InstanceHealthStatus::Unhealthy);

            health_map.entry(instance_id.to_string()).or_insert(health_status);
        }

        if health_map.is_empty() {
            return Ok(vec![]);
        }

        let ec2_resp = self
            .ec2_client
            .describe_instances()
            .set_instance_ids(Some(health_map.keys().cloned().collect()))
            .send()
            .await
            .map_err(|e| RegistrarError::Discovery(Box::new(e)))?;

        let mut instances = Vec::with_capacity(health_map.len());
        for instance in ec2_resp.reservations().iter().flat_map(|r| r.instances()) {
            let Some(instance_id) = instance.instance_id() else {
                continue;
            };
            let Some(private_ip) = instance.private_ip_address() else {
                continue;
            };
            let Some(health_status) = health_map.remove(instance_id) else {
                continue;
            };
            let launch_time = instance
                .launch_time()
                .and_then(|dt| u64::try_from(dt.secs()).ok())
                .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
            let endpoint = Url::parse(&format!("http://{private_ip}:{}", self.port))
                .map_err(|e| RegistrarError::Discovery(Box::new(e)))?;
            debug!(
                instance_id = %instance_id,
                endpoint = %endpoint,
                health = ?health_status,
                launch_time = ?launch_time,
                "discovered AWS prover instance"
            );
            instances.push(ProverInstance {
                instance_id: instance_id.to_string(),
                endpoint,
                health_status,
                launch_time,
            });
        }

        let mut missing_ids: Vec<_> = health_map.into_keys().collect();
        missing_ids.sort();
        if !missing_ids.is_empty() {
            for instance_id in &missing_ids {
                warn!(instance_id = %instance_id, "EC2 response missing data for ELB target");
            }
            return Err(RegistrarError::Discovery(Box::new(std::io::Error::other(format!(
                "EC2 response missing data for ELB target(s): {}",
                missing_ids.join(",")
            )))));
        }

        Ok(instances)
    }
}
