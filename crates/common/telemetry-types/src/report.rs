//! The `node_report` payload a node POSTs to `/v1/ingest`, and the event the ingest service writes.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Hardware, NetHealth, NodeConfigReport};

/// Major version of the node report schema this build produces and understands.
///
/// An integer rather than a string so a receiver can order versions and reject an unknown major,
/// which is the rule the schema is specified against. An exact string match cannot express it.
pub const NODE_REPORT_SCHEMA_VERSION: u16 = 1;

/// Which layer of the stack produced a report.
///
/// Standalone execution-layer and consensus-layer deployments each mint their own telemetry ID
/// and are joined server-side on the reported IP rather than coordinated in the client. The
/// combined binary removes the problem for most of the fleet.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeLayer {
    /// A `base-consensus` node.
    #[default]
    Consensus,
    /// A `base-node-reth` execution node.
    Execution,
    /// Both layers in one process.
    Combined,
}

impl NodeLayer {
    /// Returns the stable wire label for this layer.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Consensus => "consensus",
            Self::Execution => "execution",
            Self::Combined => "combined",
        }
    }
}

/// What the node does on the network.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    /// The node sequences blocks.
    Sequencer,
    /// The node derives and validates the chain from L1.
    #[default]
    Validator,
    /// The node follows another node rather than deriving for itself.
    Follower,
}

impl NodeRole {
    /// Returns the stable wire label for this role.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Sequencer => "sequencer",
            Self::Validator => "validator",
            Self::Follower => "follower",
        }
    }
}

/// Whether the address on an ingest event came from the node or from the connection.
///
/// Nodes with no advertised address, such as execution-only deployments or nodes with discovery
/// disabled, fall back to the observed edge IP so they still contribute geography.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpSource {
    /// The node advertised the address in its report.
    NodeProvided,
    /// The server used the observed edge IP of the connection.
    #[default]
    ServerObserved,
}

impl IpSource {
    /// Returns the stable wire label for this source.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NodeProvided => "node_provided",
            Self::ServerObserved => "server_observed",
        }
    }
}

/// Identity and build of the reporting client.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientMeta {
    /// Crate version of the running binary.
    pub client_version: String,
    /// Short git SHA of the build, or `unknown` when the build had no git metadata.
    pub git_sha: String,
    /// L2 chain ID the node is following.
    pub l2_chain_id: u64,
    /// Human-readable network name, used as the log facet and the S3 partition key.
    pub network: String,
    /// Which layer produced this report.
    pub layer: NodeLayer,
    /// What the node does on the network.
    pub role: NodeRole,
    /// Seconds since process start.
    pub uptime_secs: u64,
}

/// Chain head positions and the client's own view of how far behind it is.
///
/// Performance ships as head positions rather than duration distributions because a block number
/// is a scalar that merges across the fleet by counting. Duration distributions only merge if
/// every node on every release agrees on the same bucket boundaries forever.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heads {
    /// Latest unsafe head block number.
    ///
    /// Serialized as `unsafe`, which the doc's facet tables use and Rust reserves as a keyword.
    #[serde(rename = "unsafe")]
    pub unsafe_block: u64,
    /// Latest locally-derived safe head, absent until derivation has established one.
    ///
    /// This runs ahead of `safe`, which additionally requires the L1 batch to be reflected in the
    /// canonical chain. A node deriving correctly from an L1 that is itself lagging shows a
    /// healthy `local_safe` and a stalled `safe`; without both, the two look identical.
    #[serde(rename = "local_safe", default, skip_serializing_if = "Option::is_none")]
    pub local_safe_block: Option<u64>,
    /// Latest safe head block number, absent until derivation has established one.
    ///
    /// Absent rather than zero so a node that has just restarted is distinguishable from a node
    /// sitting at genesis. Both report the same number otherwise, and only one of them is broken.
    #[serde(rename = "safe", default, skip_serializing_if = "Option::is_none")]
    pub safe_block: Option<u64>,
    /// Latest finalized head block number, absent for the same reason as [`Heads::safe_block`].
    #[serde(rename = "finalized", default, skip_serializing_if = "Option::is_none")]
    pub finalized_block: Option<u64>,
    /// Seconds the unsafe head is behind wall clock, sampled when the report was built.
    ///
    /// The backend recomputes lag against the canonical head at `received_at` rather than
    /// trusting this, but keeps it, since disagreement between the two is a clock-skew signal.
    pub unsafe_latency_secs: f64,
    /// The largest `unsafe_latency_secs` seen since the previous report.
    ///
    /// This is the one value only the client can supply. A node that stalls for two minutes and
    /// recovers looks perfectly healthy in every point sample; the high-water mark, reset each
    /// report, is what makes that visible.
    pub worst_unsafe_latency_secs: f64,
    /// Every lag sample taken since the previous report, oldest first.
    ///
    /// The report interval is how often we send. This is how often we look, so one report
    /// carries a full interval of readings for a few tens of bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsafe_latency_samples: Vec<f64>,
}

/// The payload a node POSTs to `/v1/ingest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeReport {
    /// Schema version, always [`NODE_REPORT_SCHEMA_VERSION`] for reports this crate builds.
    pub schema_version: u16,
    /// Random identifier minted on first run.
    ///
    /// A UUID rather than a bare hex string because every store this lands in — Datadog, Athena,
    /// Postgres — has a native UUID type, and none of them can do anything with opaque text.
    ///
    /// Reliable within a reporting window and not across months, and nothing downstream depends
    /// on more than that. A restart preserves it because the file does; a rebuild on a fresh
    /// volume does not.
    pub telemetry_id: Uuid,
    /// Operator-supplied override used to tag our own nodes so they can be excluded from fleet
    /// numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// When the node built this report.
    pub reported_at: DateTime<Utc>,
    /// Identity and build of the reporting client.
    ///
    /// Flattened, so `client_version`, `git_sha`, `network`, `role`, and `uptime_secs` sit at the
    /// top level of the payload where the schema places them, while staying one struct in Rust.
    #[serde(flatten)]
    pub client: ClientMeta,
    /// Chain head positions and lag.
    pub heads: Heads,
    /// Runtime environment.
    pub hardware: Hardware,
    /// Allowlisted, normalized node config.
    pub config: NodeConfigReport,
    /// Peer counts, churn, and error rates.
    pub net_health: NetHealth,
}

impl Default for NodeReport {
    /// Returns an empty report stamped with the current schema version, so a report assembled
    /// with struct-update syntax can never be published under an empty version string.
    fn default() -> Self {
        Self {
            schema_version: NODE_REPORT_SCHEMA_VERSION,
            telemetry_id: Uuid::nil(),
            instance_id: None,
            reported_at: DateTime::default(),
            client: ClientMeta::default(),
            heads: Heads::default(),
            hardware: Hardware::default(),
            config: NodeConfigReport::default(),
            net_health: NetHealth::default(),
        }
    }
}

impl NodeReport {
    /// Returns whether this report declares the schema version this build understands.
    pub const fn is_current_schema(&self) -> bool {
        self.schema_version == NODE_REPORT_SCHEMA_VERSION
    }
}

/// A received report plus the fields only the server can supply.
///
/// This is the flattened shape the ingest service records. `reported_ip` is retained rather than
/// dropped after enrichment so geography can be re-derived as enrichment datasets improve, and
/// so it can serve as the correlation key when joining standalone execution and consensus
/// deployments.
///
/// Both addresses are kept. `reported_ip` is the better one to geolocate, because a node behind
/// a load balancer or inside the reporting VPC is observed at an address that says nothing about
/// where it runs. `observed_ip` is the one that cannot be forged, so keeping it is what makes a
/// spoofed or misconfigured advertisement detectable rather than invisible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeReportEvent {
    /// When ingest accepted the report.
    pub received_at: DateTime<Utc>,
    /// The address attributed to the reporting node.
    pub reported_ip: IpAddr,
    /// Whether `reported_ip` came from the node or from the connection.
    pub ip_source: IpSource,
    /// The address the connection actually arrived from.
    ///
    /// Equal to `reported_ip` whenever the node advertised nothing.
    pub observed_ip: IpAddr,
    /// The report as the node sent it.
    ///
    /// This flatten nests inside another: `NodeReport` itself flattens `ClientMeta`. Nested
    /// flatten is a known cost — serde buffers each level into an intermediate map instead of
    /// streaming, and a malformed field is reported against the outer type rather than the one
    /// that owns it. Both are accepted deliberately. The wire shape is fixed: the ingest schema,
    /// the Datadog dashboard, and the ingest tests all read these fields at the top level, so
    /// removing a level of flatten would either change the JSON or replace it with hand-written
    /// `Serialize`/`Deserialize` impls that restate every field. Buffering a payload the ingest
    /// route caps at 16 `KiB` costs nothing measurable, and the error quality only degrades on
    /// input that is already being rejected.
    #[serde(flatten)]
    pub report: NodeReport,
}

impl NodeReportEvent {
    /// Builds an event from a received report, preferring the node's advertised address and
    /// falling back to the observed edge IP.
    pub fn new(report: NodeReport, received_at: DateTime<Utc>, observed_ip: IpAddr) -> Self {
        let (reported_ip, ip_source) = report
            .net_health
            .advertised_ip
            .map_or((observed_ip, IpSource::ServerObserved), |advertised| {
                (advertised, IpSource::NodeProvided)
            });
        Self { received_at, reported_ip, ip_source, observed_ip, report }
    }

    /// Returns whether the node advertised an address other than the one it connected from.
    ///
    /// True is not by itself a fault: a node behind NAT or a proxy legitimately advertises a
    /// different address than the edge sees. It is the signal that the two need reconciling
    /// before either is trusted as the node's location.
    pub fn addresses_disagree(&self) -> bool {
        self.reported_ip != self.observed_ip
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn sample_report() -> NodeReport {
        NodeReport {
            schema_version: NODE_REPORT_SCHEMA_VERSION,
            telemetry_id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            ..Default::default()
        }
    }

    #[test]
    fn test_report_round_trips_through_json() {
        let report = sample_report();
        let encoded = serde_json::to_string(&report).expect("report should serialize");
        let decoded: NodeReport =
            serde_json::from_str(&encoded).expect("report should deserialize");
        assert_eq!(decoded, report);
        assert!(decoded.is_current_schema());
    }

    #[test]
    fn test_report_uses_snake_case_facet_names() {
        let encoded = serde_json::to_value(sample_report()).expect("report should serialize");
        assert!(encoded.get("telemetry_id").is_some(), "identity facet should be snake_case");
        assert!(
            encoded.get("client_version").is_some(),
            "client meta should be flattened to the top level, not nested under `client`"
        );
        assert!(
            encoded.get("client").is_none(),
            "the `client` wrapper must not appear on the wire"
        );
        assert!(encoded["hardware"].get("platform").is_some(), "hardware block should be present");
        assert!(
            encoded["net_health"].get("peer_count").is_some(),
            "network facet should be snake_case"
        );
        assert!(
            encoded["config"].get("p2p_enabled").is_some(),
            "config facet should be snake_case"
        );
    }

    #[test]
    fn test_absent_optional_fields_are_omitted() {
        let encoded = serde_json::to_value(sample_report()).expect("report should serialize");
        assert!(encoded.get("instance_id").is_none(), "unset instance id should be omitted");
        assert!(
            encoded["hardware"].get("cpu_model").is_none(),
            "unset cpu model should be omitted"
        );
    }

    #[test]
    fn test_event_prefers_the_advertised_address() {
        let advertised = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        let observed = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
        let mut report = sample_report();
        report.net_health.advertised_ip = Some(advertised);

        let event = NodeReportEvent::new(report, Utc::now(), observed);
        assert_eq!(event.reported_ip, advertised);
        assert_eq!(event.ip_source, IpSource::NodeProvided);
        assert_eq!(
            event.observed_ip, observed,
            "preferring the advertised address must not discard the unforgeable one"
        );
        assert!(event.addresses_disagree());
    }

    #[test]
    fn test_event_falls_back_to_the_observed_address() {
        let observed = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
        let event = NodeReportEvent::new(sample_report(), Utc::now(), observed);
        assert_eq!(event.reported_ip, observed);
        assert_eq!(event.ip_source, IpSource::ServerObserved);
        assert_eq!(event.observed_ip, observed);
        assert!(
            !event.addresses_disagree(),
            "a node that advertised nothing cannot be in disagreement with itself"
        );
    }

    #[test]
    fn test_the_payload_carries_exactly_the_specified_top_level_keys() {
        let encoded = serde_json::to_value(sample_report()).expect("report should serialize");
        let mut keys: Vec<&str> = encoded
            .as_object()
            .expect("a report is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();

        assert_eq!(
            keys,
            [
                "client_version",
                "config",
                "git_sha",
                "hardware",
                "heads",
                "l2_chain_id",
                "layer",
                "net_health",
                "network",
                "reported_at",
                "role",
                "schema_version",
                "telemetry_id",
                "uptime_secs",
            ],
            "the flattened client meta must land at the top level and nowhere else"
        );
    }

    #[test]
    fn test_event_flattens_the_report_alongside_server_fields() {
        let observed = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
        let event = NodeReportEvent::new(sample_report(), Utc::now(), observed);
        let encoded = serde_json::to_value(&event).expect("event should serialize");

        assert_eq!(encoded["ip_source"], "server_observed");
        assert!(
            encoded.get("telemetry_id").is_some(),
            "report fields should sit next to server fields, not nested under report"
        );
    }

    #[test]
    fn test_event_round_trips_through_two_levels_of_flatten() {
        let observed = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
        let event = NodeReportEvent::new(sample_report(), Utc::now(), observed);

        let encoded = serde_json::to_string(&event).expect("event should serialize");
        let decoded: NodeReportEvent =
            serde_json::from_str(&encoded).expect("event should deserialize");

        assert_eq!(
            decoded, event,
            "flattening the report into the event, and the client meta into the report, must \
             still decode back to the same value"
        );
    }

    #[test]
    fn test_the_event_carries_exactly_the_specified_top_level_keys() {
        let observed = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
        let event = NodeReportEvent::new(sample_report(), Utc::now(), observed);
        let encoded = serde_json::to_value(&event).expect("event should serialize");
        let mut keys: Vec<&str> = encoded
            .as_object()
            .expect("an event is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();

        assert_eq!(
            keys,
            [
                "client_version",
                "config",
                "git_sha",
                "hardware",
                "heads",
                "ip_source",
                "l2_chain_id",
                "layer",
                "net_health",
                "network",
                "observed_ip",
                "received_at",
                "reported_at",
                "reported_ip",
                "role",
                "schema_version",
                "telemetry_id",
                "uptime_secs",
            ],
            "the archived event is the report's keys plus exactly the four the server adds"
        );
    }

    #[test]
    fn test_server_fields_never_shadow_a_flattened_report_field() {
        // `flatten` merges keys rather than nesting them, and a duplicate key is not an error:
        // the flattened report simply overwrites the server's value and the event is archived
        // with a field the server never set. The destructure below is an exhaustiveness
        // tripwire, not a read - adding a field to `NodeReportEvent` stops compiling here until
        // the name is listed, and the assertion then proves it does not collide.
        let NodeReportEvent {
            received_at: _,
            reported_ip: _,
            ip_source: _,
            observed_ip: _,
            report: _,
        } = NodeReportEvent::new(sample_report(), Utc::now(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        const SERVER_OWNED_FIELDS: [&str; 4] =
            ["received_at", "reported_ip", "ip_source", "observed_ip"];

        let observed = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
        let report = serde_json::to_value(sample_report()).expect("report should serialize");
        let report_keys = report.as_object().expect("a report is a JSON object");
        let event =
            serde_json::to_value(NodeReportEvent::new(sample_report(), Utc::now(), observed))
                .expect("event should serialize");
        let event_keys = event.as_object().expect("an event is a JSON object");

        for field in SERVER_OWNED_FIELDS {
            assert!(
                !report_keys.contains_key(field),
                "`{field}` is owned by the server but the node also sends it, so the server's \
                 value is dropped and the archive records the node's instead"
            );
        }

        for (key, value) in report_keys {
            assert_eq!(
                event_keys.get(key),
                Some(value),
                "`{key}` did not survive flattening into the event intact"
            );
        }
    }
}
