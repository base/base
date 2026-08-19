//! AWS ALB target group instance discovery.

use std::collections::HashMap;

use aws_sdk_ec2::{Client as Ec2Client, types::Reservation};
use aws_sdk_elasticloadbalancingv2::Client as ElbClient;
use tracing::{debug, warn};
use url::Url;

use crate::{InstanceDiscovery, InstanceHealthStatus, ProverInstance, RegistrarError, Result};

/// Splits a comma-separated target-group ARN list. Empty entries are dropped.
pub fn parse_target_group_arns(raw: &str) -> Vec<String> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Discovers prover instances via AWS Elastic Load Balancing target groups.
///
/// Queries `describe_target_health` on every configured target group, unions
/// the instance IDs, then resolves each EC2 instance's private IP via
/// `describe_instances`. Health state is mapped from the ALB target health
/// state. A failure against any target group fails the whole discovery cycle so
/// the driver does not treat unseen fleets as orphans.
#[derive(Debug)]
pub struct AwsTargetGroupDiscovery {
    elb_client: ElbClient,
    ec2_client: Ec2Client,
    target_group_arns: Vec<String>,
    port: u16,
}

impl AwsTargetGroupDiscovery {
    /// Creates a new `AwsTargetGroupDiscovery` with the given AWS config.
    ///
    /// `target_group_arn` is one ARN or a comma-separated list.
    pub fn new(aws_config: &aws_config::SdkConfig, target_group_arn: String, port: u16) -> Self {
        let elb_client = ElbClient::new(aws_config);
        let ec2_client = Ec2Client::new(aws_config);
        Self {
            elb_client,
            ec2_client,
            target_group_arns: parse_target_group_arns(&target_group_arn),
            port,
        }
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
            let endpoint = Url::parse(&format!("http://{private_ip}:{port}"))
                .map_err(|e| RegistrarError::Discovery(Box::new(e)))?;
            debug!(
                instance_id = %instance_id,
                endpoint = %endpoint,
                health = ?health_status,
                "discovered AWS prover instance"
            );
            instances.push(ProverInstance {
                instance_id: instance_id.to_string(),
                endpoint,
                health_status,
            });
        }
        Ok(instances)
    }
}

impl InstanceDiscovery for AwsTargetGroupDiscovery {
    async fn discover_instances(&self) -> Result<Vec<ProverInstance>> {
        if self.target_group_arns.is_empty() {
            return Err(RegistrarError::Discovery(Box::new(std::io::Error::other(
                "no target group ARNs configured",
            ))));
        }

        let mut health_map: HashMap<String, InstanceHealthStatus> = HashMap::new();
        for arn in &self.target_group_arns {
            let elb_resp = self
                .elb_client
                .describe_target_health()
                .target_group_arn(arn)
                .send()
                .await
                .map_err(|e| RegistrarError::Discovery(Box::new(e)))?;

            for desc in elb_resp.target_health_descriptions() {
                let Some(instance_id) = desc.target().and_then(|t| t.id()) else {
                    warn!(target_group_arn = %arn, "target group entry missing instance ID, skipping");
                    continue;
                };
                if !instance_id.starts_with("i-") {
                    warn!(
                        id = %instance_id,
                        target_group_arn = %arn,
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
    use aws_sdk_ec2::types::{Instance, Reservation};
    use url::Url;

    use super::*;

    #[test]
    fn parse_target_group_arns_splits_and_trims() {
        assert_eq!(
            parse_target_group_arns(
                " arn:aws:elasticloadbalancing:us-east-1:1:targetgroup/a/abc ,arn:aws:elasticloadbalancing:us-east-1:1:targetgroup/b/def, "
            ),
            vec![
                "arn:aws:elasticloadbalancing:us-east-1:1:targetgroup/a/abc",
                "arn:aws:elasticloadbalancing:us-east-1:1:targetgroup/b/def",
            ]
        );
        assert!(parse_target_group_arns(" , , ").is_empty());
        assert_eq!(
            parse_target_group_arns("arn:aws:elasticloadbalancing:us-east-1:1:targetgroup/a/abc"),
            vec!["arn:aws:elasticloadbalancing:us-east-1:1:targetgroup/a/abc"]
        );
    }

    fn reservation(instances: Vec<Instance>) -> Reservation {
        Reservation::builder().set_instances(Some(instances)).build()
    }

    fn instance(id: &str, private_ip: Option<&str>) -> Instance {
        Instance::builder()
            .instance_id(id)
            .set_private_ip_address(private_ip.map(str::to_string))
            .build()
    }

    #[test]
    fn assemble_prover_instances_preserves_ec2_and_elb_data() {
        let reservations = vec![reservation(vec![
            instance("i-001", Some("10.0.0.1")),
            instance("i-002", Some("10.0.0.2")),
            instance("i-003", Some("10.0.0.3")),
            instance("i-004", Some("10.0.0.4")),
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
        assert_eq!(instances[1].instance_id, "i-002");
        assert_eq!(instances[1].endpoint, Url::parse("http://10.0.0.2:9000").unwrap());
        assert_eq!(instances[1].health_status, InstanceHealthStatus::Initial);
        assert_eq!(instances[2].health_status, InstanceHealthStatus::Unhealthy);
        assert_eq!(instances[3].health_status, InstanceHealthStatus::Draining);
    }

    #[test]
    fn assemble_prover_instances_returns_url_parse_error() {
        let reservations = vec![reservation(vec![instance("i-001", Some("bad host"))])];
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
            instance("i-001", Some("10.0.0.1")),
            instance("i-002", None),
            instance("i-999", Some("10.0.0.9")),
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
