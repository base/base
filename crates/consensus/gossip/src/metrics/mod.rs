//! Metrics for the Gossip stack.

base_metrics::define_metrics! {
    base_node

    #[describe("Events received by the gossip protocol")]
    #[label(name = "type", default = ["message", "subscribed", "unsubscribed", "slow_peer", "not_supported"])]
    gossip_event: gauge,

    #[describe("Events received by the libp2p gossipsub Swarm")]
    #[label(
        name = "type",
        default = [
            "subscribed",
            "unsubscribed",
            "gossipsub_not_supported",
            "slow_peer",
            "message_received"
        ]
    )]
    gossipsub_event: gauge,

    #[describe("Connections made to the libp2p Swarm")]
    #[label(name = "type", default = ["connected", "outgoing_error", "incoming_error", "closed"])]
    gossipsub_connection: gauge,

    #[describe("Number of OpNetworkPayloadEnvelope gossipped out through the libp2p Swarm")]
    unsafe_block_published: gauge,

    #[describe("Number of peers connected to the libp2p gossip Swarm")]
    gossip_peer_count: gauge,

    #[describe("Number of peers dialed by the libp2p Swarm")]
    dial_peer: gauge,

    #[describe("Number of errors when dialing peers")]
    #[label(
        name = "type",
        default = [
            "invalid_enr",
            "already_connected",
            "connection_error",
            "invalid_multiaddr",
            "already_dialing",
            "threshold_reached",
            "blocked_peer",
            "blocked_address",
            "blocked_subnet"
        ]
    )]
    dial_peer_error: gauge,

    #[describe("Calls made to the Gossip RPC module")]
    #[label(
        name = "method",
        default = [
            "opp2p_self",
            "opp2p_peerCount",
            "opp2p_peers",
            "opp2p_peerStats",
            "opp2p_discoveryTable",
            "opp2p_blockPeer",
            "opp2p_listBlockedPeers",
            "opp2p_blockAddr",
            "opp2p_unblockAddr",
            "opp2p_listBlockedAddrs",
            "opp2p_blockSubnet",
            "opp2p_unblockSubnet",
            "opp2p_listBlockedSubnets",
            "opp2p_protectPeer",
            "opp2p_unprotectPeer",
            "opp2p_connectPeer",
            "opp2p_disconnectPeer"
        ]
    )]
    rpc_calls: gauge,

    #[describe("Number of peers banned by the gossip stack")]
    banned_peers: gauge,

    #[describe("Observations of peer scores in the gossipsub mesh")]
    peer_scores: histogram,

    #[describe("Duration of peer connections in seconds")]
    gossip_peer_connection_duration_seconds: histogram,

    #[describe("Total number of block validation attempts")]
    block_validation_total: counter,

    #[describe("Number of successful block validations")]
    block_validation_success: counter,

    #[describe("Number of failed block validations by reason")]
    #[label(
        name = "reason",
        default = [
            "timestamp_future",
            "timestamp_past",
            "invalid_hash",
            "invalid_signature",
            "invalid_signer",
            "too_many_blocks",
            "block_seen",
            "invalid_block",
            "parent_beacon_root",
            "blob_gas_used",
            "excess_blob_gas",
            "withdrawals_root"
        ]
    )]
    block_validation_failed: counter,

    #[describe("Duration of block validation in seconds")]
    block_validation_duration_seconds: histogram,

    #[describe("Distribution of block versions")]
    #[label(name = "version", default = ["v1", "v2", "v3", "v4"])]
    block_version: counter,
}
