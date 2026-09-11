//! The `net_health.*` block: peer counts, churn, and gossip/request error rates.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// The `net_health.*` block, present on every report event.
///
/// This describes the reporting node's own view of the network, not its peers. Per-peer detail
/// is deliberately excluded: it describes other operators' nodes, who never saw an opt-out
/// prompt, and a per-peer `user_agent` would yield a version distribution for nodes that opted
/// out, which is a crawler by another route.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetHealth {
    /// Peers currently connected.
    pub peer_count: u32,
    /// Configured target peer count, when the node has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_target: Option<u32>,
    /// Nodes known to the discovery table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovered_count: Option<u32>,
    /// Peers that connected since the previous report.
    pub peers_joined: u32,
    /// Peers that disconnected since the previous report.
    pub peers_left: u32,
    /// The node's own libp2p peer identifier. Never the peer ID secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    /// The node's own Ethereum node record, as it is already published to the p2p network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enr: Option<String>,
    /// The address the node advertises to peers, when it advertises one.
    ///
    /// Reported so the backend can compare it against the observed edge IP. Disagreement is the
    /// ground truth `basectl doctor` needs to tell an operator their connectivity is
    /// misconfigured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_ip: Option<IpAddr>,
    /// Gossip messages rejected or errored, as a fraction of those seen since the last report.
    ///
    /// Absent rather than zero when the client cannot measure it, so a client that never
    /// learned the rate is distinguishable from one reporting a clean interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gossip_error_rate: Option<f64>,
    /// p2p requests that failed, as a fraction of those issued since the last report.
    ///
    /// Absent rather than zero for the same reason as [`NetHealth::gossip_error_rate`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_error_rate: Option<f64>,
}
