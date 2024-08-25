#![allow(missing_docs)]
//! Network RPC types

use alloy_primitives::ChainId;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::IpAddr};

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
    pub connectedness: u8,
    /// 0: "Unknown", 1: "Inbound" (if the peer contacted us)
    /// 2: "Outbound" (if we connected to them)
    pub direction: u8,
    pub protected: bool,
    #[serde(rename = "chainID")]
    pub chain_id: ChainId,
    /// nanosecond
    pub latency: u64,
    pub gossip_blocks: bool,
    #[serde(rename = "scores")]
    pub peer_scores: PeerScores,
}

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
