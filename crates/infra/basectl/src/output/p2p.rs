//! P2P info output shapes and table rendering.

use serde::Serialize;

use super::KeyValueTable;
use crate::{DiscoveryInfo, NodeEndpoint, NodeInfoReport, PeerStatsReport};

/// Humanized execution-layer JSON for `basectl p2p info --json`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct P2pElInfoJson {
    /// Advertised public IP address.
    pub advertised_ip: Option<String>,
    /// Advertised TCP port.
    pub tcp_port: Option<u16>,
    /// Discovery configuration summary.
    pub discovery: Option<DiscoveryInfo>,
    /// Full `enode://` URL advertised by the node.
    pub enode: Option<String>,
    /// Connected peer count.
    pub peer_count: Option<u32>,
}

/// Humanized consensus-layer JSON for `basectl p2p info --json`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct P2pClInfoJson {
    /// Advertised public IP address.
    pub advertised_ip: Option<String>,
    /// Advertised TCP port.
    pub tcp_port: Option<u16>,
    /// Discovery configuration summary.
    pub discovery: Option<DiscoveryInfo>,
    /// Full `enr:` record advertised by the node.
    pub enr: Option<String>,
    /// libp2p peer ID of the node.
    pub peer_id: Option<String>,
    /// Multiaddrs the node reports listening on.
    pub addresses: Vec<String>,
    /// Connected peer count.
    pub peer_count: Option<u32>,
    /// Configured maximum peer count.
    pub max_peer_count: Option<u32>,
}

/// Humanized JSON payload for `basectl p2p info --json`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct P2pInfoJson {
    /// Selected basectl network/config name.
    pub network: String,
    /// Execution-layer p2p summary.
    pub el: P2pElInfoJson,
    /// Consensus-layer p2p summary.
    pub cl: P2pClInfoJson,
}

impl P2pInfoJson {
    /// Builds the humanized JSON payload for `basectl p2p info --json`.
    pub fn from_report(
        network: &str,
        report: &NodeInfoReport,
        peer_stats: &PeerStatsReport,
    ) -> Self {
        let endpoint_fields = |endpoint: Option<NodeEndpoint>| {
            endpoint.map_or((None, None, None), |ep| {
                (Some(ep.advertised_ip.to_string()), Some(ep.tcp_port), Some(ep.discovery))
            })
        };
        let (el_advertised_ip, el_tcp_port, el_discovery) = endpoint_fields(report.el);
        let (cl_advertised_ip, cl_tcp_port, cl_discovery) = endpoint_fields(report.cl);
        Self {
            network: network.to_string(),
            el: P2pElInfoJson {
                advertised_ip: el_advertised_ip,
                tcp_port: el_tcp_port,
                discovery: el_discovery,
                enode: report.el_identity.enode.clone(),
                peer_count: peer_stats.el_count,
            },
            cl: P2pClInfoJson {
                advertised_ip: cl_advertised_ip,
                tcp_port: cl_tcp_port,
                discovery: cl_discovery,
                enr: report.cl_identity.enr.clone(),
                peer_id: report.cl_identity.peer_id.clone(),
                addresses: report.cl_identity.addresses.clone(),
                peer_count: peer_stats.cl.map(|stats| stats.connected),
                max_peer_count: peer_stats.cl.and_then(|stats| stats.max_peer_count),
            },
        }
    }
}

/// Pretty-table builder for `basectl p2p info`.
#[derive(Debug, Clone, Copy, Default)]
pub struct P2pInfoTable;

impl P2pInfoTable {
    /// Builds the pretty `KeyValueTable` for `basectl p2p info`.
    pub fn from_report(
        network: &str,
        report: &NodeInfoReport,
        peer_stats: &PeerStatsReport,
    ) -> KeyValueTable {
        let mut table = KeyValueTable::new();
        let format_discovery_flags =
            |v4_enabled: bool, v5_enabled: bool| match (v4_enabled, v5_enabled) {
                (true, true) => "v4+v5",
                (true, false) => "v4",
                (false, true) => "v5",
                (false, false) => "disabled",
            };
        let unavailable_admin_method =
            |method: &str| format!("unavailable (`{method}` not exposed by this RPC)");
        let unavailable_el_peer_count =
            || "unavailable (`net_peerCount` not exposed by this RPC)".to_string();
        let unavailable_cl_endpoint =
            || "unavailable (could not parse advertised endpoint from `opp2p_self`)".to_string();
        let unavailable_cl_self =
            || "unavailable (`opp2p_self` not exposed by this RPC)".to_string();
        let unavailable_cl_peer_stats =
            || "unavailable (`opp2p_peerStats` not exposed by this RPC)".to_string();
        let unavailable_cl_max_peer_count =
            || "unavailable (CL node did not report max peer count)".to_string();
        let unavailable_cl_max_peer_count_without_rpc = || {
            "unavailable (`opp2p_peerStats` not exposed by this RPC; cannot read max peer count)"
                .to_string()
        };
        let cl_missing = |specific: &str| {
            if report.cl_identity.peer_id.is_some() {
                specific.to_string()
            } else {
                unavailable_cl_self()
            }
        };

        let el_discovery = report.el.map(|endpoint| {
            format_discovery_flags(endpoint.discovery.v4_enabled, endpoint.discovery.v5_enabled)
                .to_string()
        });
        let cl_discovery = report.cl.map(|endpoint| {
            format_discovery_flags(endpoint.discovery.v4_enabled, endpoint.discovery.v5_enabled)
                .to_string()
        });
        let (cl_peer_count, cl_max_peer_count) = peer_stats.cl.map_or_else(
            || (unavailable_cl_peer_stats(), unavailable_cl_max_peer_count_without_rpc()),
            |stats| {
                (
                    stats.connected.to_string(),
                    stats
                        .max_peer_count
                        .map(|count| count.to_string())
                        .unwrap_or_else(unavailable_cl_max_peer_count),
                )
            },
        );

        table
            .row("network", network)
            .row(
                "el_advertised_ip",
                report
                    .el
                    .map(|endpoint| endpoint.advertised_ip.to_string())
                    .unwrap_or_else(|| unavailable_admin_method("admin_nodeInfo")),
            )
            .row(
                "el_p2p_port",
                report
                    .el
                    .map(|endpoint| endpoint.tcp_port.to_string())
                    .unwrap_or_else(|| unavailable_admin_method("admin_nodeInfo")),
            )
            .row(
                "el_discovery_port",
                report
                    .el
                    .map(|endpoint| endpoint.discovery.udp_port.to_string())
                    .unwrap_or_else(|| unavailable_admin_method("admin_nodeInfo")),
            )
            .row(
                "el_discovery",
                el_discovery.unwrap_or_else(|| unavailable_admin_method("admin_nodeInfo")),
            )
            .row(
                "el_enode",
                report
                    .el_identity
                    .enode
                    .clone()
                    .unwrap_or_else(|| unavailable_admin_method("admin_nodeInfo")),
            )
            .row(
                "el_peer_count",
                peer_stats
                    .el_count
                    .map(|count| count.to_string())
                    .unwrap_or_else(unavailable_el_peer_count),
            )
            .row(
                "cl_advertised_ip",
                report
                    .cl
                    .map(|endpoint| endpoint.advertised_ip.to_string())
                    .unwrap_or_else(unavailable_cl_endpoint),
            )
            .row(
                "cl_p2p_port",
                report
                    .cl
                    .map(|endpoint| endpoint.tcp_port.to_string())
                    .unwrap_or_else(unavailable_cl_endpoint),
            )
            .row(
                "cl_discovery_port",
                report
                    .cl
                    .map(|endpoint| endpoint.discovery.udp_port.to_string())
                    .unwrap_or_else(unavailable_cl_endpoint),
            )
            .row("cl_discovery", cl_discovery.unwrap_or_else(unavailable_cl_endpoint))
            .row(
                "cl_enr",
                report.cl_identity.enr.clone().unwrap_or_else(|| {
                    cl_missing("unavailable (CL node did not advertise an ENR)")
                }),
            )
            .row(
                "cl_peer_id",
                report.cl_identity.peer_id.clone().unwrap_or_else(unavailable_cl_self),
            )
            .row(
                "cl_addresses",
                if report.cl_identity.addresses.is_empty() {
                    cl_missing("unavailable (CL node did not report listen addresses)")
                } else {
                    report.cl_identity.addresses.join(", ")
                },
            )
            .row("cl_peer_count", cl_peer_count)
            .row("cl_max_peer_count", cl_max_peer_count);

        table
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{P2pInfoJson, P2pInfoTable};
    use crate::{
        ClNodeIdentity, DiscoveryInfo, ElNodeIdentity, NodeEndpoint, NodeInfoReport,
        PeerStatsReport,
    };

    #[test]
    fn info_json_keeps_max_peer_count_key_when_absent() {
        let report = NodeInfoReport::default();
        let peer_stats = PeerStatsReport { el_count: None, cl: None };

        let info =
            serde_json::to_value(P2pInfoJson::from_report("devnet", &report, &peer_stats)).unwrap();

        assert_eq!(info["el"]["peerCount"], serde_json::Value::Null);
        assert_eq!(info["cl"]["peerCount"], serde_json::Value::Null);
        assert_eq!(info["cl"]["maxPeerCount"], serde_json::Value::Null);
    }

    #[test]
    fn info_json_includes_cl_max_peer_count() {
        let report = NodeInfoReport {
            el: None,
            cl: Some(NodeEndpoint {
                advertised_ip: "203.0.113.10".parse().unwrap(),
                tcp_port: 9000,
                discovery: DiscoveryInfo { udp_port: 9001, v4_enabled: true, v5_enabled: false },
            }),
            ..Default::default()
        };
        let peer_stats = PeerStatsReport {
            el_count: Some(8),
            cl: Some(
                serde_json::from_value(json!({
                    "connected": 17,
                    "table": 20,
                    "blocksTopic": 1,
                    "blocksTopicV2": 2,
                    "blocksTopicV3": 3,
                    "blocksTopicV4": 4,
                    "banned": 0,
                    "known": 21,
                    "maxPeerCount": 100,
                }))
                .unwrap(),
            ),
        };

        let info =
            serde_json::to_value(P2pInfoJson::from_report("devnet", &report, &peer_stats)).unwrap();

        assert_eq!(info["cl"]["maxPeerCount"], json!(100));
        assert!(!info["el"].as_object().unwrap().contains_key("maxPeerCount"));
    }

    #[test]
    fn info_table_renders_specific_max_peer_count_message_when_rpc_missing() {
        let report = NodeInfoReport::default();
        let peer_stats = PeerStatsReport { el_count: None, cl: None };

        let mut buf = Vec::new();
        P2pInfoTable::from_report("devnet", &report, &peer_stats).render(&mut buf).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        let line = rendered
            .lines()
            .find(|line| line.contains("cl_max_peer_count"))
            .expect("missing cl_max_peer_count row");

        assert!(line.contains("cannot read max peer count"), "unexpected row: {line}");
    }

    #[test]
    fn info_table_renders_specific_max_peer_count_message_when_field_missing() {
        let report = NodeInfoReport {
            el: None,
            cl: Some(NodeEndpoint {
                advertised_ip: "203.0.113.10".parse().unwrap(),
                tcp_port: 9000,
                discovery: DiscoveryInfo { udp_port: 9001, v4_enabled: true, v5_enabled: false },
            }),
            ..Default::default()
        };
        let peer_stats = PeerStatsReport {
            el_count: None,
            cl: Some(
                serde_json::from_value(json!({
                    "connected": 17,
                    "table": 20,
                    "blocksTopic": 1,
                    "blocksTopicV2": 2,
                    "blocksTopicV3": 3,
                    "blocksTopicV4": 4,
                    "banned": 0,
                    "known": 21,
                }))
                .unwrap(),
            ),
        };

        let mut buf = Vec::new();
        P2pInfoTable::from_report("devnet", &report, &peer_stats).render(&mut buf).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        let line = rendered
            .lines()
            .find(|line| line.contains("cl_max_peer_count"))
            .expect("missing cl_max_peer_count row");

        assert!(line.contains("did not report max peer count"), "unexpected row: {line}");
    }

    #[test]
    fn info_json_includes_raw_identity_records() {
        let report = NodeInfoReport {
            el_identity: ElNodeIdentity {
                enode: Some("enode://abc@203.0.113.10:30303".to_string()),
            },
            cl_identity: ClNodeIdentity {
                enr: Some("enr:-cl-record".to_string()),
                peer_id: Some("16Uiu2HAmTestPeerId".to_string()),
                addresses: vec!["/ip4/203.0.113.10/tcp/9222".to_string()],
            },
            ..Default::default()
        };
        let peer_stats = PeerStatsReport { el_count: None, cl: None };

        let info =
            serde_json::to_value(P2pInfoJson::from_report("devnet", &report, &peer_stats)).unwrap();

        assert_eq!(info["el"]["enode"], json!("enode://abc@203.0.113.10:30303"));
        assert!(!info["el"].as_object().unwrap().contains_key("enr"));
        assert_eq!(info["cl"]["enr"], json!("enr:-cl-record"));
        assert_eq!(info["cl"]["peerId"], json!("16Uiu2HAmTestPeerId"));
        assert_eq!(info["cl"]["addresses"], json!(["/ip4/203.0.113.10/tcp/9222"]));
    }

    #[test]
    fn info_table_renders_raw_identity_records() {
        let report = NodeInfoReport {
            el_identity: ElNodeIdentity {
                enode: Some("enode://abc@203.0.113.10:30303".to_string()),
            },
            cl_identity: ClNodeIdentity {
                enr: Some("enr:-cl-record".to_string()),
                peer_id: Some("16Uiu2HAmTestPeerId".to_string()),
                addresses: vec![
                    "/ip4/203.0.113.10/tcp/9222".to_string(),
                    "/ip4/127.0.0.1/tcp/9222".to_string(),
                ],
            },
            ..Default::default()
        };
        let peer_stats = PeerStatsReport { el_count: None, cl: None };

        let mut buf = Vec::new();
        P2pInfoTable::from_report("devnet", &report, &peer_stats).render(&mut buf).unwrap();
        let rendered = String::from_utf8(buf).unwrap();

        let row_value = |key: &str| {
            rendered
                .lines()
                .find(|line| line.starts_with(key))
                .unwrap_or_else(|| panic!("missing {key} row"))
                .trim_start_matches(key)
                .trim()
                .to_string()
        };
        assert_eq!(row_value("el_enode"), "enode://abc@203.0.113.10:30303");
        assert_eq!(row_value("cl_enr"), "enr:-cl-record");
        assert_eq!(row_value("cl_peer_id"), "16Uiu2HAmTestPeerId");
        assert_eq!(
            row_value("cl_addresses"),
            "/ip4/203.0.113.10/tcp/9222, /ip4/127.0.0.1/tcp/9222"
        );

        let mut buf = Vec::new();
        P2pInfoTable::from_report("devnet", &NodeInfoReport::default(), &peer_stats)
            .render(&mut buf)
            .unwrap();
        let unavailable = String::from_utf8(buf).unwrap();
        let row_line = |key: &str| {
            unavailable
                .lines()
                .find(|line| line.starts_with(key))
                .unwrap_or_else(|| panic!("missing {key} row"))
                .to_string()
        };
        assert!(row_line("el_enode").contains("admin_nodeInfo"));
        assert!(row_line("cl_enr").contains("opp2p_self"));
        assert!(row_line("cl_peer_id").contains("opp2p_self"));
        assert!(row_line("cl_addresses").contains("opp2p_self"));
    }
}
