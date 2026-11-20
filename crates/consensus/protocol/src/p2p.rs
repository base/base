//! Wire types used in the consensus P2P RPC API.

use alloc::{string::String, vec::Vec};
use core::net::IpAddr;

use alloy_primitives::{ChainId, map::HashMap};
use derive_more::Display;

/// The peer info.
///
/// <https://github.com/ethereum-optimism/optimism/blob/develop/op-node/p2p/rpc_api.go#L15>
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    /// The peer id.
    #[serde(rename = "peerID")]
    pub peer_id: String,
    /// The node id.
    #[serde(rename = "nodeID")]
    pub node_id: String,
    /// The user agent.
    pub user_agent: String,
    /// The protocol version.
    pub protocol_version: String,
    /// The enr for the peer.
    /// If the peer is not in the discovery table, this will not be set.
    #[serde(rename = "ENR")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enr: Option<String>,
    /// The peer addresses.
    pub addresses: Vec<String>,
    /// Peer supported protocols
    pub protocols: Option<Vec<String>>,
    /// 0: "`NotConnected`",
    /// 1: "Connected",
    /// 2: "`CanConnect`" (gracefully disconnected)
    /// 3: "`CannotConnect`" (tried but failed)
    pub connectedness: Connectedness,
    /// 0: "Unknown",
    /// 1: "Inbound" (if the peer contacted us)
    /// 2: "Outbound" (if we connected to them)
    pub direction: Direction,
    /// Whether the peer is protected.
    pub protected: bool,
    /// The chain id.
    #[serde(rename = "chainID")]
    pub chain_id: ChainId,
    /// The peer latency in nanoseconds
    pub latency: u64,
    /// Whether the peer gossips
    pub gossip_blocks: bool,
    /// The peer scores.
    #[serde(rename = "scores")]
    pub peer_scores: PeerScores,
}

/// `GossipSub` topic-specific scoring metrics.
///
/// Reference: <https://github.com/ethereum-optimism/optimism/blob/8dd17a7b114a7c25505cd2e15ce4e3d0f7e3f7c1/op-node/p2p/store/iface.go#L13>
#[derive(Clone, Default, Debug, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicScores {
    /// Duration the peer has participated in the topic mesh.
    pub time_in_mesh: f64,
    /// Count of first-time message deliveries from this peer.
    pub first_message_deliveries: f64,
    /// Count of messages delivered while in the mesh topology.
    pub mesh_message_deliveries: f64,
    /// Count of invalid or malicious messages from this peer.
    pub invalid_message_deliveries: f64,
}

/// Comprehensive `GossipSub` scoring metrics for peer quality assessment.
///
/// Reference: <https://github.com/ethereum-optimism/optimism/blob/8dd17a7b114a7c25505cd2e15ce4e3d0f7e3f7c1/op-node/p2p/store/iface.go#L20C6-L20C18>
#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GossipScores {
    /// Aggregate score across all scoring dimensions.
    pub total: f64,
    /// Block-specific topic scores for consensus messages.
    pub blocks: TopicScores,
    /// Penalty for IP address colocation with other peers.
    #[serde(rename = "IPColocationFactor")]
    pub ip_colocation_factor: f64,
    /// Penalty for problematic behavior patterns.
    pub behavioral_penalty: f64,
}

/// Legacy request-response scoring fields retained for RPC response compatibility.
///
/// Reference: <https://github.com/ethereum-optimism/optimism/blob/8dd17a7b114a7c25505cd2e15ce4e3d0f7e3f7c1/op-node/p2p/store/iface.go#L31C1-L35C2>
#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReqRespScores {
    /// Number of valid responses provided by this peer.
    pub valid_responses: f64,
    /// Number of error responses or failed requests.
    pub error_responses: f64,
    /// Number of rejected payloads.
    pub rejected_payloads: f64,
}

/// Peer Scores
///
/// <https://github.com/ethereum-optimism/optimism/blob/8dd17a7b114a7c25505cd2e15ce4e3d0f7e3f7c1/op-node/p2p/store/iface.go#L81>
#[derive(Clone, Default, Debug, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerScores {
    /// The gossip scores
    pub gossip: GossipScores,
    /// The request-response scores.
    pub req_resp: ReqRespScores,
}

/// Peer count data.
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCount {
    /// The total number of connected peers to the discovery service.
    pub connected_discovery: Option<usize>,
    /// The total number of connected peers to the gossip service.
    pub connected_gossip: usize,
}

/// A raw peer dump.
///
/// <https://github.com/ethereum-optimism/optimism/blob/40750a58e7a4a6f06370d18dfe6c6eab309012d9/op-node/p2p/rpc_api.go#L36>
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDump {
    /// The total number of connected peers
    pub total_connected: u32,
    /// A map from peer id to peer info
    pub peers: HashMap<String, PeerInfo>,
    /// A list of banned peers.
    pub banned_peers: Vec<String>,
    /// A list of banned ip addresses.
    #[serde(rename = "bannedIPS")]
    pub banned_ips: Vec<IpAddr>,
    /// The banned subnets
    pub banned_subnets: Vec<ipnet::IpNet>,
}

/// Peer stats.
///
/// <https://github.com/ethereum-optimism/optimism/blob/develop/op-node/p2p/rpc_server.go#L203>
#[derive(Clone, Default, Debug, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStats {
    /// The number of connections
    pub connected: u32,
    /// The table.
    pub table: u32,
    /// The blocks topic.
    #[serde(rename = "blocksTopic")]
    pub blocks_topic: u32,
    /// The blocks v2 topic.
    #[serde(rename = "blocksTopicV2")]
    pub blocks_topic_v2: u32,
    /// The blocks v3 topic.
    #[serde(rename = "blocksTopicV3")]
    pub blocks_topic_v3: u32,
    /// The blocks v4 topic.
    #[serde(rename = "blocksTopicV4")]
    pub blocks_topic_v4: u32,
    /// The banned count.
    pub banned: u32,
    /// The known count.
    pub known: u32,
    /// Configured maximum number of established libp2p connections.
    ///
    /// This is an additive Base extension to `opp2p_peerStats`. Older nodes may
    /// omit this field when they do not expose connection limits through that
    /// RPC method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_peer_count: Option<u32>,
}

/// Represents the connectivity state of a peer in a network, indicating the reachability and
/// interaction status of a node with its peers.
#[derive(Clone, Debug, Display, PartialEq, Eq, Copy, Default)]
#[repr(u8)]
pub enum Connectedness {
    /// No current connection to the peer, and no recent history of a successful connection.
    #[default]
    #[display("Not Connected")]
    NotConnected = 0,
    /// An active, open connection to the peer exists.
    #[display("Connected")]
    Connected = 1,
    /// Connection to the peer is possible but not currently established; usually implies a past
    /// successful connection.
    #[display("Can Connect")]
    CanConnect = 2,
    /// Recent attempts to connect to the peer failed, indicating potential issues in reachability
    /// or peer status.
    #[display("Cannot Connect")]
    CannotConnect = 3,
    /// Connection to the peer is limited; may not have full capabilities.
    #[display("Limited")]
    Limited = 4,
}

impl From<u8> for Connectedness {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::Connected,
            2 => Self::CanConnect,
            3 => Self::CannotConnect,
            4 => Self::Limited,
            _ => Self::NotConnected,
        }
    }
}

impl serde::Serialize for Connectedness {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> serde::Deserialize<'de> for Connectedness {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        match value {
            0 => Ok(Self::NotConnected),
            1 => Ok(Self::Connected),
            2 => Ok(Self::CanConnect),
            3 => Ok(Self::CannotConnect),
            4 => Ok(Self::Limited),
            _ => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Unsigned(u64::from(value)),
                &"a value between 0 and 4",
            )),
        }
    }
}

/// Direction represents the direction of a connection.
#[derive(Debug, Clone, Display, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Unknown is the default direction when the direction is not specified.
    #[default]
    #[display("Unknown")]
    Unknown = 0,
    /// Inbound is for when the remote peer initiated the connection.
    #[display("Inbound")]
    Inbound = 1,
    /// Outbound is for when the local peer initiated the connection.
    #[display("Outbound")]
    Outbound = 2,
}

impl serde::Serialize for Direction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> serde::Deserialize<'de> for Direction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        match value {
            0 => Ok(Self::Unknown),
            1 => Ok(Self::Inbound),
            2 => Ok(Self::Outbound),
            _ => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Unsigned(u64::from(value)),
                &"a value between 0 and 2",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_info_connectedness_and_direction_roundtrip() {
        let peer_info = PeerInfo {
            peer_id: String::from("peer123"),
            node_id: String::from("node123"),
            user_agent: String::from("agent/1.0"),
            protocol_version: String::from("1.0"),
            enr: Some(String::from("enr123")),
            addresses: alloc::vec![String::from("127.0.0.1:8080")],
            protocols: Some(alloc::vec![String::from("eth/66")]),
            connectedness: Connectedness::Connected,
            direction: Direction::Inbound,
            protected: true,
            chain_id: 1,
            latency: 100,
            gossip_blocks: true,
            peer_scores: PeerScores {
                gossip: GossipScores {
                    total: 1.0,
                    blocks: TopicScores {
                        time_in_mesh: 10.0,
                        first_message_deliveries: 5.0,
                        mesh_message_deliveries: 2.0,
                        invalid_message_deliveries: 0.0,
                    },
                    ip_colocation_factor: 0.5,
                    behavioral_penalty: 0.1,
                },
                req_resp: ReqRespScores {
                    valid_responses: 10.0,
                    error_responses: 1.0,
                    rejected_payloads: 0.0,
                },
            },
        };

        let serialized = serde_json::to_string(&peer_info).expect("Serialization failed");
        let deserialized: PeerInfo =
            serde_json::from_str(&serialized).expect("Deserialization failed");

        assert_eq!(peer_info.connectedness, deserialized.connectedness);
        assert_eq!(peer_info.direction, deserialized.direction);
        assert_eq!(peer_info.peer_id, deserialized.peer_id);
    }

    #[test]
    fn test_peer_stats_deserialize_legacy_payload_without_connection_limit() {
        let stats: PeerStats = serde_json::from_value(serde_json::json!({
            "connected": 7,
            "table": 9,
            "blocksTopic": 1,
            "blocksTopicV2": 2,
            "blocksTopicV3": 3,
            "blocksTopicV4": 4,
            "banned": 0,
            "known": 11,
        }))
        .unwrap();

        assert_eq!(stats.connected, 7);
        assert_eq!(stats.known, 11);
        assert_eq!(stats.max_peer_count, None);
    }
}
