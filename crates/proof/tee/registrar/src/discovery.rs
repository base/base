//! K8s `StatefulSet` and AWS ALB target group instance discovery.

use std::collections::HashMap;

use async_trait::async_trait;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_elasticloadbalancingv2::Client as ElbClient;
use tracing::{debug, warn};

use crate::{InstanceDiscovery, InstanceHealthStatus, ProverInstance, RegistrarError, Result};

/// Discovers prover pods by enumerating a K8s `StatefulSet`'s deterministic DNS names.
///
/// `StatefulSet` pods receive stable DNS names of the form:
/// `{name}-{i}.{svc}.{ns}.svc.cluster.local`
///
/// Discovery requires no API calls — pod endpoints are fully deterministic
/// from the `StatefulSet` name, headless service name, namespace, replica count,
/// and port alone. K8s pod readiness gates proposal traffic independently;
/// the registrar polls every enumerated pod each cycle regardless.
#[derive(Debug, Clone)]
pub struct K8sStatefulSetDiscovery {
    statefulset_name: String,
    service_name: String,
    namespace: String,
    replicas: usize,
    port: u16,
}

impl K8sStatefulSetDiscovery {
    /// Creates a new discovery instance for the given `StatefulSet`.
    pub const fn new(
        statefulset_name: String,
        service_name: String,
        namespace: String,
        replicas: usize,
        port: u16,
    ) -> Self {
        Self { statefulset_name, service_name, namespace, replicas, port }
    }

    /// Returns the pod DNS endpoint for replica index `i`.
    ///
    /// Format: `{name}-{i}.{svc}.{ns}.svc.cluster.local:{port}`
    pub fn pod_endpoint(&self, i: usize) -> String {
        format!(
            "{}-{}.{}.{}.svc.cluster.local:{}",
            self.statefulset_name, i, self.service_name, self.namespace, self.port
        )
    }
}

#[async_trait]
impl InstanceDiscovery for K8sStatefulSetDiscovery {
    /// Returns one [`ProverInstance`] per replica, all marked [`InstanceHealthStatus::Healthy`].
    ///
    /// Unlike the AWS path where the ALB provides real health status, K8s discovery
    /// has no liveness signal — pod DNS names are deterministic regardless of
    /// whether a pod is actually running. The registrar's downstream poll loop
    /// (`ProverClient`) handles unreachable pods by returning connection errors,
    /// which the driver treats as per-instance failures without stopping the cycle.
    async fn discover_instances(&self) -> Result<Vec<ProverInstance>> {
        let instances = (0..self.replicas)
            .map(|i| {
                let endpoint = self.pod_endpoint(i);
                debug!(pod = %endpoint, "discovered prover pod");
                ProverInstance {
                    instance_id: endpoint.clone(),
                    endpoint,
                    health_status: InstanceHealthStatus::Healthy,
                }
            })
            .collect();
        Ok(instances)
    }
}

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
    /// Creates a new `AwsTargetGroupDiscovery` with the given AWS clients.
    pub const fn new(
        elb_client: ElbClient,
        ec2_client: Ec2Client,
        target_group_arn: String,
        port: u16,
    ) -> Self {
        Self { elb_client, ec2_client, target_group_arn, port }
    }

    /// Assembles [`ProverInstance`] objects from ELB target data and EC2 IP data.
    ///
    /// Returns **all** discovered instances regardless of health status, matching
    /// the behavior of `K8sStatefulSetDiscovery` which also returns every replica.
    /// The caller (registration driver) decides which instances to act on based on
    /// [`InstanceHealthStatus::should_register`].
    ///
    /// Targets with no matching IP entry are silently dropped (this can happen if
    /// an instance was terminated between the ELB and EC2 calls).
    ///
    /// This function is a pure transformation — no AWS SDK calls are made — which
    /// makes it straightforwardly unit-testable without SDK mocks.
    pub fn assemble_prover_instances(
        targets: &[(String, InstanceHealthStatus)],
        instance_ips: &HashMap<String, String>,
        port: u16,
    ) -> Vec<ProverInstance> {
        targets
            .iter()
            .filter_map(|(instance_id, health_status)| {
                let private_ip = instance_ips.get(instance_id)?;
                let endpoint = format!("{private_ip}:{port}");
                debug!(
                    instance_id = %instance_id,
                    endpoint = %endpoint,
                    health = ?health_status,
                    "discovered AWS prover instance"
                );
                Some(ProverInstance {
                    instance_id: instance_id.clone(),
                    endpoint,
                    health_status: *health_status,
                })
            })
            .collect()
    }
}

#[async_trait]
impl InstanceDiscovery for AwsTargetGroupDiscovery {
    async fn discover_instances(&self) -> Result<Vec<ProverInstance>> {
        // Query ELB for current target health.
        let elb_resp = self
            .elb_client
            .describe_target_health()
            .target_group_arn(&self.target_group_arn)
            .send()
            .await
            .map_err(|e| RegistrarError::Discovery(Box::new(e)))?;

        // Extract (instance_id, health_status) pairs from the ELB response.
        // Validates that targets are instance-type (id starts with "i-") and
        // deduplicates instances registered on multiple ports (first-seen wins).
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

            if let std::collections::hash_map::Entry::Vacant(e) =
                health_map.entry(instance_id.to_string())
            {
                e.insert(health_status);
            } else {
                debug!(
                    instance_id = %instance_id,
                    "instance registered on multiple ports; keeping first-seen health status"
                );
            }
        }

        let mut targets: Vec<(String, InstanceHealthStatus)> = health_map.into_iter().collect();
        targets.sort_by(|(a, _), (b, _)| a.cmp(b));

        if targets.is_empty() {
            return Ok(vec![]);
        }

        // Collect all instance IDs for EC2 lookup (not just registerable ones),
        // so that discover_instances returns the full set — consistent with
        // K8sStatefulSetDiscovery which returns all replicas.
        let all_ids: Vec<String> = targets.iter().map(|(id, _)| id.clone()).collect();

        // Resolve private IPs for all instances in a single EC2 call.
        let ec2_resp = self
            .ec2_client
            .describe_instances()
            .set_instance_ids(Some(all_ids))
            .send()
            .await
            .map_err(|e| RegistrarError::Discovery(Box::new(e)))?;

        let instance_ips: HashMap<String, String> = ec2_resp
            .reservations()
            .iter()
            .flat_map(|r| r.instances())
            .filter_map(|i| {
                let id = i.instance_id()?.to_string();
                let ip = i.private_ip_address()?.to_string();
                Some((id, ip))
            })
            .collect();

        // Warn about instances that the EC2 call didn't return
        // (e.g. terminated between the ELB and EC2 calls, or missing a private IP).
        for (id, _) in &targets {
            if !instance_ips.contains_key(id) {
                warn!(instance_id = %id, "instance missing from EC2 response, skipping");
            }
        }

        Ok(Self::assemble_prover_instances(&targets, &instance_ips, self.port))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn four_replica_discovery() -> K8sStatefulSetDiscovery {
        K8sStatefulSetDiscovery::new(
            "prover".into(),
            "prover-headless".into(),
            "provers".into(),
            4,
            8000,
        )
    }

    #[rstest]
    #[case::ordinal_zero(0, "prover-0.prover-headless.provers.svc.cluster.local:8000")]
    #[case::last_ordinal(3, "prover-3.prover-headless.provers.svc.cluster.local:8000")]
    fn pod_endpoint_format(#[case] ordinal: usize, #[case] expected: &str) {
        assert_eq!(four_replica_discovery().pod_endpoint(ordinal), expected);
    }

    #[rstest]
    #[case::zero(0)]
    #[case::one(1)]
    #[case::four(4)]
    #[tokio::test]
    async fn discover_instances_returns_one_per_replica(#[case] replicas: usize) {
        let d = K8sStatefulSetDiscovery::new(
            "prover".into(),
            "prover-headless".into(),
            "provers".into(),
            replicas,
            8000,
        );
        let instances = d.discover_instances().await.unwrap();
        assert_eq!(instances.len(), replicas);
    }

    #[tokio::test]
    async fn all_discovered_instances_are_healthy() {
        let instances = four_replica_discovery().discover_instances().await.unwrap();
        assert!(instances.iter().all(|i| i.health_status == InstanceHealthStatus::Healthy));
    }

    #[tokio::test]
    async fn instance_ids_match_pod_endpoints() {
        let d = four_replica_discovery();
        let instances = d.discover_instances().await.unwrap();
        for (i, inst) in instances.iter().enumerate() {
            assert_eq!(inst.instance_id, d.pod_endpoint(i));
        }
    }

    #[tokio::test]
    async fn endpoints_match_pod_endpoints() {
        let d = four_replica_discovery();
        let instances = d.discover_instances().await.unwrap();
        for (i, inst) in instances.iter().enumerate() {
            assert_eq!(inst.endpoint, d.pod_endpoint(i));
        }
    }

    mod aws {
        use super::*;

        fn make_ips(pairs: &[(&str, &str)]) -> HashMap<String, String> {
            pairs.iter().map(|(id, ip)| (id.to_string(), ip.to_string())).collect()
        }

        fn make_targets(
            pairs: &[(&str, InstanceHealthStatus)],
        ) -> Vec<(String, InstanceHealthStatus)> {
            pairs.iter().map(|(id, s)| (id.to_string(), *s)).collect()
        }

        #[rstest]
        #[case::healthy("i-001", "10.0.0.1", InstanceHealthStatus::Healthy)]
        #[case::initial("i-002", "10.0.0.2", InstanceHealthStatus::Initial)]
        #[case::unhealthy("i-003", "10.0.0.3", InstanceHealthStatus::Unhealthy)]
        #[case::draining("i-004", "10.0.0.4", InstanceHealthStatus::Draining)]
        fn assemble_single_instance_preserves_status(
            #[case] id: &str,
            #[case] ip: &str,
            #[case] status: InstanceHealthStatus,
        ) {
            let targets = make_targets(&[(id, status)]);
            let ips = make_ips(&[(id, ip)]);
            let instances =
                AwsTargetGroupDiscovery::assemble_prover_instances(&targets, &ips, 8000);
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance_id, id);
            assert_eq!(instances[0].endpoint, format!("{ip}:8000"));
            assert_eq!(instances[0].health_status, status);
        }

        #[test]
        fn assemble_drops_instance_missing_from_ip_map() {
            let targets = make_targets(&[("i-005", InstanceHealthStatus::Healthy)]);
            let ips = HashMap::new();
            let instances =
                AwsTargetGroupDiscovery::assemble_prover_instances(&targets, &ips, 8000);
            assert!(instances.is_empty());
        }

        #[test]
        fn assemble_empty_targets_returns_empty() {
            let instances =
                AwsTargetGroupDiscovery::assemble_prover_instances(&[], &HashMap::new(), 8000);
            assert!(instances.is_empty());
        }

        #[test]
        fn assemble_port_appears_in_endpoint() {
            let targets = make_targets(&[("i-006", InstanceHealthStatus::Healthy)]);
            let ips = make_ips(&[("i-006", "10.0.0.6")]);
            let instances =
                AwsTargetGroupDiscovery::assemble_prover_instances(&targets, &ips, 9999);
            assert_eq!(instances[0].endpoint, "10.0.0.6:9999");
        }

        #[test]
        fn assemble_returns_all_statuses() {
            let targets = make_targets(&[
                ("i-010", InstanceHealthStatus::Healthy),
                ("i-011", InstanceHealthStatus::Initial),
                ("i-012", InstanceHealthStatus::Unhealthy),
                ("i-013", InstanceHealthStatus::Draining),
            ]);
            let ips = make_ips(&[
                ("i-010", "10.0.1.0"),
                ("i-011", "10.0.1.1"),
                ("i-012", "10.0.1.2"),
                ("i-013", "10.0.1.3"),
            ]);
            let instances =
                AwsTargetGroupDiscovery::assemble_prover_instances(&targets, &ips, 8000);
            assert_eq!(instances.len(), 4);
        }
    }
}
