#![allow(missing_docs)]
//! Network RPC types

use crate::collections::HashMap;
use alloc::{string::String, vec::Vec};
use alloy_primitives::ChainId;
use core::net::IpAddr;
use serde::{
    de::{self, Unexpected},
    Deserialize, Deserializer, Serialize, Serializer,
};

// https://github.com/ethereum-optimism/optimism/blob/8dd17a7b114a7c25505cd2e15ce4e3d0f7e3f7c1/op-node/p2p/store/iface.go#L13
#[derive(Clone, Debug, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicScores {
    pub time_in_mesh: f64,
    pub first_message_deliveries: f64,
    pub mesh_message_deliveries: f64,
    pub invalid_message_deliveries: f64,
}

// https://github.com/ethereum-optimism/optimism/blob/8dd17a7b114a7c25505cd2e15ce4e3d0f7e3f7c1/op-node/p2p/store/iface.go#L20C6-L20C18
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GossipScores {
    pub total: f64,
    pub blocks: TopicScores,
    #[serde(rename = "IPColocationFactor")]
    pub ip_colocation_factor: f64,
    pub behavioral_penalty: f64,
}

// https://github.com/ethereum-optimism/optimism/blob/8dd17a7b114a7c25505cd2e15ce4e3d0f7e3f7c1/op-node/p2p/store/iface.go#L31C1-L35C2
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReqRespScores {
    pub valid_responses: f64,
    pub error_responses: f64,
    pub rejected_payloads: f64,
}

// https://github.com/ethereum-optimism/optimism/blob/8dd17a7b114a7c25505cd2e15ce4e3d0f7e3f7c1/op-node/p2p/store/iface.go#L81
#[derive(Clone, Debug, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerScores {
    pub gossip: GossipScores,
    pub req_resp: ReqRespScores,
}

// https://github.com/ethereum-optimism/optimism/blob/develop/op-node/p2p/rpc_api.go#L15
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    #[serde(rename = "peerID")]
    pub peer_id: String,
    #[serde(rename = "nodeID")]
    pub node_id: String,
    pub user_agent: String,
    pub protocol_version: String,
    #[serde(rename = "ENR")]
    pub enr: String,
    pub addresses: Vec<String>,
    pub protocols: Option<Vec<String>>,
    /// 0: "`NotConnected`", 1: "Connected",
    /// 2: "`CanConnect`" (gracefully disconnected)
    /// 3: "`CannotConnect`" (tried but failed)
    pub connectedness: Connectedness,
    /// 0: "Unknown", 1: "Inbound" (if the peer contacted us)
    /// 2: "Outbound" (if we connected to them)
    pub direction: Direction,
    pub protected: bool,
    #[serde(rename = "chainID")]
    pub chain_id: ChainId,
    /// nanosecond
    pub latency: u64,
    pub gossip_blocks: bool,
    #[serde(rename = "scores")]
    pub peer_scores: PeerScores,
}
// https://github.com/ethereum-optimism/optimism/blob/40750a58e7a4a6f06370d18dfe6c6eab309012d9/op-node/p2p/rpc_api.go#L36
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDump {
    pub total_connected: u32,
    pub peers: HashMap<String, PeerInfo>,
    pub banned_peers: Vec<String>,
    #[serde(rename = "bannedIPS")]
    pub banned_ips: Vec<IpAddr>,
    // todo: should be IPNet
    pub banned_subnets: Vec<IpAddr>,
}

// https://github.com/ethereum-optimism/optimism/blob/develop/op-node/p2p/rpc_server.go#L203
#[derive(Clone, Debug, Copy, Serialize, Deserialize)]
pub struct PeerStats {
    pub connected: u32,
    pub table: u32,
    #[serde(rename = "blocksTopic")]
    pub blocks_topic: u32,
    #[serde(rename = "blocksTopicV2")]
    pub blocks_topic_v2: u32,
    #[serde(rename = "blocksTopicV3")]
    pub blocks_topic_v3: u32,
    pub banned: u32,
    pub known: u32,
}

/// Represents the connectivity state of a peer in a network, indicating the reachability and
/// interaction status of a node with its peers.
#[derive(Clone, Debug, PartialEq, Copy, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Connectedness {
    /// No current connection to the peer, and no recent history of a successful connection.
    #[default]
    NotConnected = 0,

    /// An active, open connection to the peer exists.
    Connected = 1,

    /// Connection to the peer is possible but not currently established; usually implies a past
    /// successful connection.
    CanConnect = 2,

    /// Recent attempts to connect to the peer failed, indicating potential issues in reachability
    /// or peer status.
    CannotConnect = 3,

    /// Connection to the peer is limited; may not have full capabilities.
    Limited = 4,
}

impl core::fmt::Display for Connectedness {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Connectedness::NotConnected => write!(f, "Not Connected"),
            Connectedness::Connected => write!(f, "Connected"),
            Connectedness::CanConnect => write!(f, "Can Connect"),
            Connectedness::CannotConnect => write!(f, "Cannot Connect"),
            Connectedness::Limited => write!(f, "Limited"),
        }
    }
}

impl From<u8> for Connectedness {
    fn from(value: u8) -> Self {
        match value {
            0 => Connectedness::NotConnected,
            1 => Connectedness::Connected,
            2 => Connectedness::CanConnect,
            3 => Connectedness::CannotConnect,
            4 => Connectedness::Limited,
            _ => Connectedness::NotConnected,
        }
    }
}
/// Direction represents the direction of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Unknown is the default direction when the direction is not specified.
    #[default]
    Unknown = 0,
    /// Inbound is for when the remote peer initiated the connection.
    Inbound = 1,
    /// Outbound is for when the local peer initiated the connection.
    Outbound = 2,
}

impl Serialize for Direction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for Direction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        match value {
            0 => Ok(Direction::Unknown),
            1 => Ok(Direction::Inbound),
            2 => Ok(Direction::Outbound),
            _ => Err(de::Error::invalid_value(
                Unexpected::Unsigned(value as u64),
                &"a value between 0 and 2",
            )),
        }
    }
}
impl core::fmt::Display for Direction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Direction::Unknown => "Unknown",
                Direction::Inbound => "Inbound",
                Direction::Outbound => "Outbound",
            }
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{self};

    #[test]
    fn test_direction_serialization() {
        assert_eq!(
            serde_json::to_string(&Direction::Unknown).unwrap(),
            "0",
            "Serialization failed for Direction::Unknown"
        );
        assert_eq!(
            serde_json::to_string(&Direction::Inbound).unwrap(),
            "1",
            "Serialization failed for Direction::Inbound"
        );
        assert_eq!(
            serde_json::to_string(&Direction::Outbound).unwrap(),
            "2",
            "Serialization failed for Direction::Outbound"
        );
    }

    #[test]
    fn test_direction_deserialization() {
        let unknown: Direction = serde_json::from_str("0").unwrap();
        let inbound: Direction = serde_json::from_str("1").unwrap();
        let outbound: Direction = serde_json::from_str("2").unwrap();

        assert_eq!(unknown, Direction::Unknown, "Deserialization mismatch for Direction::Unknown");
        assert_eq!(inbound, Direction::Inbound, "Deserialization mismatch for Direction::Inbound");
        assert_eq!(
            outbound,
            Direction::Outbound,
            "Deserialization mismatch for Direction::Outbound"
        );
    }
    #[test]
    fn test_peer_info_connectedness_serialization() {
        let peer_info = PeerInfo {
            peer_id: String::from("peer123"),
            node_id: String::from("node123"),
            user_agent: String::from("MyUserAgent"),
            protocol_version: String::from("v1"),
            enr: String::from("enr123"),
            addresses: vec![String::from("127.0.0.1")],
            protocols: Some(vec![String::from("eth"), String::from("p2p")]),
            connectedness: Connectedness::Connected,
            direction: Direction::Outbound,
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

        assert_eq!(peer_info.peer_id, deserialized.peer_id);
        assert_eq!(peer_info.node_id, deserialized.node_id);
        assert_eq!(peer_info.user_agent, deserialized.user_agent);
        assert_eq!(peer_info.protocol_version, deserialized.protocol_version);
        assert_eq!(peer_info.enr, deserialized.enr);
        assert_eq!(peer_info.addresses, deserialized.addresses);
        assert_eq!(peer_info.protocols, deserialized.protocols);
        assert_eq!(peer_info.connectedness, deserialized.connectedness);
        assert_eq!(peer_info.direction, deserialized.direction);
        assert_eq!(peer_info.protected, deserialized.protected);
        assert_eq!(peer_info.chain_id, deserialized.chain_id);
        assert_eq!(peer_info.latency, deserialized.latency);
        assert_eq!(peer_info.gossip_blocks, deserialized.gossip_blocks);
        assert_eq!(peer_info.peer_scores.gossip.total, deserialized.peer_scores.gossip.total);
        assert_eq!(
            peer_info.peer_scores.req_resp.valid_responses,
            deserialized.peer_scores.req_resp.valid_responses
        );
    }
}
