//! Raft conductor membership responses.

use alloc::{string::String, vec::Vec};
use serde::{Deserialize, Serialize};

/// Live Raft cluster membership snapshot returned by `conductor_clusterMembership`.
///
/// Mirrors the upstream op-conductor `ClusterMembership` struct.
/// See: <https://github.com/ethereum-optimism/optimism/blob/develop/op-conductor/consensus/iface.go>
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterMembership {
    /// All servers currently known to the Raft configuration.
    pub servers: Vec<ServerInfo>,
    /// Raft configuration index — increments on every membership change.
    pub version: u64,
}

/// A single member of the Raft cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Stable Raft server identifier (e.g. `"sequencer-1"`).
    ///
    /// Matches the `server_id` operators configure per node and the parameter
    /// expected by `conductor_transferLeaderToServer`.
    pub id: String,
    /// Raft binary-protocol address (e.g. `"op-conductor-1:5051"`).
    ///
    /// **NOT** the JSON-RPC URL — the Raft consensus port is distinct from the
    /// HTTP RPC port. Callers that need to talk JSON-RPC to a peer must derive
    /// the RPC URL separately (typically by extracting the host and applying a
    /// port template from local configuration).
    pub addr: String,
    /// Whether this server can vote in Raft elections.
    pub suffrage: ServerSuffrage,
}

/// Raft suffrage (voting eligibility) for a cluster member.
///
/// Wire format is an integer (`0` = voter, `1` = nonvoter), matching the
/// upstream `ServerSuffrage` enum in op-conductor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum ServerSuffrage {
    /// Server is eligible to vote in Raft elections.
    Voter,
    /// Server is a non-voting member (replica only).
    Nonvoter,
}

impl TryFrom<u8> for ServerSuffrage {
    type Error = UnknownServerSuffrage;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Voter),
            1 => Ok(Self::Nonvoter),
            other => Err(UnknownServerSuffrage(other)),
        }
    }
}

impl From<ServerSuffrage> for u8 {
    fn from(value: ServerSuffrage) -> Self {
        match value {
            ServerSuffrage::Voter => 0,
            ServerSuffrage::Nonvoter => 1,
        }
    }
}

/// Error returned when an unknown `ServerSuffrage` discriminant is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown server suffrage discriminant: {0}")]
pub struct UnknownServerSuffrage(pub u8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_suffrage_wire_format_is_numeric() {
        assert_eq!(serde_json::to_string(&ServerSuffrage::Voter).unwrap(), "0");
        assert_eq!(serde_json::to_string(&ServerSuffrage::Nonvoter).unwrap(), "1");
        assert_eq!(serde_json::from_str::<ServerSuffrage>("0").unwrap(), ServerSuffrage::Voter);
        assert_eq!(serde_json::from_str::<ServerSuffrage>("1").unwrap(), ServerSuffrage::Nonvoter);
        assert!(serde_json::from_str::<ServerSuffrage>("2").is_err());
    }

    #[test]
    fn cluster_membership_round_trips_upstream_wire_format() {
        let json = r#"{
            "servers": [
                {"id": "sequencer-1", "addr": "10.0.1.10:50050", "suffrage": 0},
                {"id": "sequencer-2", "addr": "10.0.1.11:50050", "suffrage": 0},
                {"id": "sequencer-3", "addr": "10.0.1.12:50050", "suffrage": 1}
            ],
            "version": 42
        }"#;
        let membership: ClusterMembership = serde_json::from_str(json).unwrap();
        assert_eq!(membership.version, 42);
        assert_eq!(membership.servers.len(), 3);
        assert_eq!(membership.servers[0].id, "sequencer-1");
        assert_eq!(membership.servers[0].addr, "10.0.1.10:50050");
        assert_eq!(membership.servers[0].suffrage, ServerSuffrage::Voter);
        assert_eq!(membership.servers[2].suffrage, ServerSuffrage::Nonvoter);

        let reserialized = serde_json::to_value(&membership).unwrap();
        let original: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(reserialized, original);
    }
}
