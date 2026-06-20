//! AWS ALB target group instance discovery.

use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

use aws_sdk_ec2::{Client as Ec2Client, types::Reservation};
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

    /// Builds prover instances from EC2 reservations and removes matched IDs from `health_map`.
    pub fn assemble_prover_instances(
        reservations: &[Reservation],
        health_map: &mut HashMap<String, InstanceHealthStatus>,
        port: u16,
    ) -> Result<Vec<ProverInstance>> {
        let mut instances = Vec::with_capacity(health_map.len());
        for instance in reservations.iter().flat_map(|r| r.instances()) {
            let Some(instance_id) = instance.instance_id() else {
                continue;
            };
            let Some(private_ip) = instance.private_ip_address() else {
                warn!(instance_id = %instance_id, "EC2 instance present but missing private IP");
                continue;
            };
            let Some(health_status) = health_map.remove(instance_id) else {
                continue;
            };
            let launch_time = instance
                .launch_time()
                .and_then(|dt| u64::try_from(dt.secs()).ok())
                .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
            let endpoint = Url::parse(&format!("http://{private_ip}:{port}"))
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
        Ok(instances)
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
                .map(|s| match s.as_str() {
                    "initial" => InstanceHealthStatus::Initial,
                    "healthy" => InstanceHealthStatus::Healthy,
                    "draining" => InstanceHealthStatus::Draining,
                    _ => InstanceHealthStatus::Unhealthy,
                })
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

        let instances =
            Self::assemble_prover_instances(ec2_resp.reservations(), &mut health_map, self.port)?;

        let mut missing_ids: Vec<_> = health_map.into_keys().collect();
        missing_ids.sort();
        if !missing_ids.is_empty() {
            for instance_id in &missing_ids {
                warn!(instance_id = %instance_id, "EC2 response missing or incomplete data for ELB target");
            }
            return Err(RegistrarError::Discovery(Box::new(std::io::Error::other(format!(
                "EC2 response missing or incomplete data for ELB target(s): {}",
                missing_ids.join(",")
            )))));
        }

        Ok(instances)
    }
}

#[cfg(test)]
mod tests {
    use aws_sdk_ec2::{
        primitives::DateTime,
        types::{Instance, Reservation},
    };
    use url::Url;

    use super::*;

    fn reservation(instances: Vec<Instance>) -> Reservation {
        Reservation::builder().set_instances(Some(instances)).build()
    }

    fn instance(id: &str, private_ip: Option<&str>, launch_time_secs: Option<i64>) -> Instance {
        Instance::builder()
            .instance_id(id)
            .set_private_ip_address(private_ip.map(str::to_string))
            .set_launch_time(launch_time_secs.map(DateTime::from_secs))
            .build()
    }

    #[test]
    fn assemble_prover_instances_preserves_ec2_and_elb_data() {
        let launch_secs = 1_700_000_000;
        let launch_time = SystemTime::UNIX_EPOCH + Duration::from_secs(launch_secs as u64);
        let reservations = vec![reservation(vec![
            instance("i-001", Some("10.0.0.1"), Some(launch_secs)),
            instance("i-002", Some("10.0.0.2"), None),
            instance("i-003", Some("10.0.0.3"), None),
            instance("i-004", Some("10.0.0.4"), None),
        ])];
        let mut health_map = HashMap::from([
            ("i-001".to_string(), InstanceHealthStatus::Healthy),
            ("i-002".to_string(), InstanceHealthStatus::Initial),
            ("i-003".to_string(), InstanceHealthStatus::Unhealthy),
            ("i-004".to_string(), InstanceHealthStatus::Draining),
        ]);

        let instances = AwsTargetGroupDiscovery::assemble_prover_instances(
            &reservations,
            &mut health_map,
            9000,
        )
        .unwrap();

        assert!(health_map.is_empty());
        assert_eq!(instances.len(), 4);
        assert_eq!(instances[0].instance_id, "i-001");
        assert_eq!(instances[0].endpoint, Url::parse("http://10.0.0.1:9000").unwrap());
        assert_eq!(instances[0].health_status, InstanceHealthStatus::Healthy);
        assert_eq!(instances[0].launch_time, Some(launch_time));
        assert_eq!(instances[1].instance_id, "i-002");
        assert_eq!(instances[1].endpoint, Url::parse("http://10.0.0.2:9000").unwrap());
        assert_eq!(instances[1].health_status, InstanceHealthStatus::Initial);
        assert_eq!(instances[1].launch_time, None);
        assert_eq!(instances[2].health_status, InstanceHealthStatus::Unhealthy);
        assert_eq!(instances[3].health_status, InstanceHealthStatus::Draining);
    }

    #[test]
    fn assemble_prover_instances_returns_url_parse_error() {
        let reservations = vec![reservation(vec![instance("i-001", Some("bad host"), None)])];
        let mut health_map = HashMap::from([("i-001".to_string(), InstanceHealthStatus::Healthy)]);

        let err = AwsTargetGroupDiscovery::assemble_prover_instances(
            &reservations,
            &mut health_map,
            9000,
        )
        .unwrap_err();

        let RegistrarError::Discovery(source) = err else {
            panic!("expected discovery error");
        };
        assert!(source.downcast_ref::<url::ParseError>().is_some());
    }

    #[test]
    fn assemble_prover_instances_leaves_missing_ec2_data_in_health_map() {
        let reservations = vec![reservation(vec![
            instance("i-001", Some("10.0.0.1"), None),
            instance("i-002", None, None),
            instance("i-999", Some("10.0.0.9"), None),
        ])];
        let mut health_map = HashMap::from([
            ("i-001".to_string(), InstanceHealthStatus::Healthy),
            ("i-002".to_string(), InstanceHealthStatus::Initial),
            ("i-003".to_string(), InstanceHealthStatus::Draining),
        ]);

        let instances = AwsTargetGroupDiscovery::assemble_prover_instances(
            &reservations,
            &mut health_map,
            8000,
        )
        .unwrap();
        let mut missing_ids: Vec<_> = health_map.into_keys().collect();
        missing_ids.sort();

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].instance_id, "i-001");
        assert_eq!(missing_ids, vec!["i-002", "i-003"]);
    }
}
