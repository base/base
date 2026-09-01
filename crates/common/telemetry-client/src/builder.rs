//! Assembly of a full [`NodeReport`] from the node's live state.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use base_telemetry_types::{
    ClientMeta, Hardware, Heads, NODE_REPORT_SCHEMA_VERSION, NetHealth, NodeConfigReport,
    NodeLayer, NodeReport, NodeRole,
};
use chrono::Utc;

use crate::{GIT_SHA, HardwareCollector, TelemetryId};

/// Everything about the node that is fixed for the lifetime of the process.
///
/// Assembled once at startup and handed to [`NodeReportBuilder`], so a report can never
/// disagree with itself about which node produced it.
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    /// The persistent per-node identifier.
    pub telemetry_id: TelemetryId,
    /// An operator-supplied tag, set on the nodes we run ourselves so they can be excluded
    /// from fleet counts.
    pub instance_id: Option<String>,
    /// The release version of the node binary.
    pub client_version: String,
    /// The L2 chain the node follows.
    pub l2_chain_id: u64,
    /// The human-readable network name, e.g. `mainnet`.
    pub network: String,
    /// Which layer this process runs.
    pub layer: NodeLayer,
    /// What the node does on the network.
    pub role: NodeRole,
    /// The data directory, used to size the disk the node actually writes to.
    pub data_dir: Option<PathBuf>,
}

/// Builds one [`NodeReport`] per reporting interval.
///
/// Hardware is re-read on every build rather than cached at startup: a disk filling up is one
/// of the failures this telemetry exists to see, and the cost is a handful of small reads
/// against `sysfs` and `procfs` once every reporting interval.
#[derive(Debug, Clone)]
pub struct NodeReportBuilder {
    identity: NodeIdentity,
    node_config: NodeConfigReport,
    started_at: Instant,
}

impl NodeReportBuilder {
    /// Creates a builder, starting the uptime clock now.
    pub fn new(identity: NodeIdentity, node_config: NodeConfigReport) -> Self {
        Self { identity, node_config, started_at: Instant::now() }
    }

    /// Returns how long the node has been running, in whole seconds.
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Returns the runtime environment as it looks right now.
    pub fn hardware(&self) -> Hardware {
        HardwareCollector::collect(self.identity.data_dir.as_deref())
    }

    /// Returns the fixed client metadata, stamped with the current uptime.
    pub fn client_meta(&self) -> ClientMeta {
        ClientMeta {
            client_version: self.identity.client_version.clone(),
            git_sha: GIT_SHA.to_string(),
            l2_chain_id: self.identity.l2_chain_id,
            network: self.identity.network.clone(),
            layer: self.identity.layer,
            role: self.identity.role,
            uptime_secs: self.uptime().as_secs(),
        }
    }

    /// Assembles a report from the caller's head and network snapshots.
    pub fn build(&self, heads: Heads, net_health: NetHealth) -> NodeReport {
        NodeReport {
            schema_version: NODE_REPORT_SCHEMA_VERSION,
            telemetry_id: self.identity.telemetry_id.uuid(),
            instance_id: self.identity.instance_id.clone(),
            reported_at: Utc::now(),
            client: self.client_meta(),
            heads,
            hardware: self.hardware(),
            config: self.node_config.clone(),
            net_health,
        }
    }
}

#[cfg(test)]
mod tests {
    use base_telemetry_types::{HardwarePlatform, PruneMode};

    use super::*;
    use crate::LatencySampler;

    fn identity() -> NodeIdentity {
        NodeIdentity {
            telemetry_id: TelemetryId::generate(),
            instance_id: None,
            client_version: "1.2.3".to_string(),
            l2_chain_id: 8453,
            network: "mainnet".to_string(),
            layer: NodeLayer::Consensus,
            role: NodeRole::Validator,
            data_dir: None,
        }
    }

    fn node_config() -> NodeConfigReport {
        NodeConfigReport { prune_mode: Some(PruneMode::Archive), ..Default::default() }
    }

    #[test]
    fn test_build_stamps_the_current_schema_and_identity() {
        let identity = identity();
        let telemetry_id = identity.telemetry_id.uuid();
        let builder = NodeReportBuilder::new(identity, node_config());

        let report = builder.build(Heads::default(), NetHealth::default());

        assert!(report.is_current_schema());
        assert_eq!(report.telemetry_id, telemetry_id);
        assert_eq!(report.client.client_version, "1.2.3");
        assert_eq!(report.client.git_sha, GIT_SHA);
        assert_eq!(report.client.l2_chain_id, 8453);
        assert_eq!(report.config.prune_mode, Some(PruneMode::Archive));
    }

    #[test]
    fn test_instance_id_is_carried_through() {
        let builder = NodeReportBuilder::new(
            NodeIdentity { instance_id: Some("base-us-east-1".to_string()), ..identity() },
            node_config(),
        );

        let report = builder.build(Heads::default(), NetHealth::default());

        assert_eq!(report.instance_id.as_deref(), Some("base-us-east-1"));
    }

    #[test]
    fn test_hardware_is_collected_on_every_build() {
        let builder = NodeReportBuilder::new(identity(), node_config());

        let report = builder.build(Heads::default(), NetHealth::default());

        assert_eq!(report.hardware.os, std::env::consts::OS);
        assert_eq!(report.hardware.arch, std::env::consts::ARCH);
        if !cfg!(target_os = "linux") {
            assert_eq!(report.hardware.platform, HardwarePlatform::Unknown);
        }
    }

    #[test]
    fn test_latency_window_supplies_the_head_lag_fields() {
        let builder = NodeReportBuilder::new(identity(), node_config());
        let mut sampler = LatencySampler::new(8);
        sampler.record(1.0);
        sampler.record(9.0);
        sampler.record(2.0);

        let mut heads = Heads { unsafe_block: 100, safe_block: Some(90), ..Default::default() };
        sampler.drain().apply(&mut heads);
        let report = builder.build(heads, NetHealth::default());

        assert_eq!(report.heads.unsafe_block, 100);
        assert_eq!(report.heads.unsafe_latency_secs, 2.0);
        assert_eq!(
            report.heads.worst_unsafe_latency_secs, 9.0,
            "the high-water mark must survive a later, healthier sample"
        );
        assert_eq!(report.heads.unsafe_latency_samples, vec![1.0, 9.0, 2.0]);
    }
}
