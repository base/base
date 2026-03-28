//! Metrics for the Gossip stack.

base_metrics::define_metrics! {
    base_node

    #[describe("Events received by the gossip protocol")]
    #[label(r#type)]
    gossip_event: gauge,

    #[describe("Events received by the libp2p gossipsub Swarm")]
    #[label(r#type)]
    gossipsub_event: gauge,

    #[describe("Connections made to the libp2p Swarm")]
    #[label(r#type)]
    gossipsub_connection: gauge,

    #[describe("Number of OpNetworkPayloadEnvelope gossipped out through the libp2p Swarm")]
    unsafe_block_published: gauge,

    #[describe("Number of peers connected to the libp2p gossip Swarm")]
    gossip_peer_count: gauge,

    #[describe("Number of peers dialed by the libp2p Swarm")]
    dial_peer: gauge,

    #[describe("Number of errors when dialing peers")]
    #[label(r#type)]
    dial_peer_error: gauge,

    #[describe("Calls made to the Gossip RPC module")]
    #[label(method)]
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
    #[label(reason)]
    block_validation_failed: counter,

    #[describe("Duration of block validation in seconds")]
    block_validation_duration_seconds: histogram,

    #[describe("Distribution of block versions")]
    #[label(version)]
    block_version: counter,
}

impl Metrics {
    /// Initializes metrics for the Gossip stack.
    ///
    /// This does two things:
    /// * Describes various metrics.
    /// * Initializes metrics to 0 so they can be queried immediately.
    pub fn init() {
        Self::describe();
        Self::zero();
    }

    /// Initializes metrics to `0` so they can be queried immediately by consumers of prometheus
    /// metrics.
    pub fn zero() {
        // RPC Calls
        Self::rpc_calls("opp2p_self").set(0);
        Self::rpc_calls("opp2p_peerCount").set(0);
        Self::rpc_calls("opp2p_peers").set(0);
        Self::rpc_calls("opp2p_peerStats").set(0);
        Self::rpc_calls("opp2p_discoveryTable").set(0);
        Self::rpc_calls("opp2p_blockPeer").set(0);
        Self::rpc_calls("opp2p_listBlockedPeers").set(0);
        Self::rpc_calls("opp2p_blockAddr").set(0);
        Self::rpc_calls("opp2p_unblockAddr").set(0);
        Self::rpc_calls("opp2p_listBlockedAddrs").set(0);
        Self::rpc_calls("opp2p_blockSubnet").set(0);
        Self::rpc_calls("opp2p_unblockSubnet").set(0);
        Self::rpc_calls("opp2p_listBlockedSubnets").set(0);
        Self::rpc_calls("opp2p_protectPeer").set(0);
        Self::rpc_calls("opp2p_unprotectPeer").set(0);
        Self::rpc_calls("opp2p_connectPeer").set(0);
        Self::rpc_calls("opp2p_disconnectPeer").set(0);

        // Gossip Events
        Self::gossip_event("message").set(0);
        Self::gossip_event("subscribed").set(0);
        Self::gossip_event("unsubscribed").set(0);
        Self::gossip_event("slow_peer").set(0);
        Self::gossip_event("not_supported").set(0);

        // Peer dials
        Self::dial_peer().set(0);
        Self::dial_peer_error("invalid_enr").set(0);
        Self::dial_peer_error("already_connected").set(0);
        Self::dial_peer_error("connection_error").set(0);
        Self::dial_peer_error("invalid_multiaddr").set(0);
        Self::dial_peer_error("already_dialing").set(0);
        Self::dial_peer_error("threshold_reached").set(0);
        Self::dial_peer_error("blocked_peer").set(0);
        Self::dial_peer_error("blocked_address").set(0);
        Self::dial_peer_error("blocked_subnet").set(0);

        // Unsafe Blocks
        Self::unsafe_block_published().set(0);

        // Peer Counts
        Self::gossip_peer_count().set(0);

        // Connection
        Self::gossipsub_connection("connected").set(0);
        Self::gossipsub_connection("outgoing_error").set(0);
        Self::gossipsub_connection("incoming_error").set(0);
        Self::gossipsub_connection("closed").set(0);

        // Gossipsub Events
        Self::gossipsub_event("subscribed").set(0);
        Self::gossipsub_event("unsubscribed").set(0);
        Self::gossipsub_event("gossipsub_not_supported").set(0);
        Self::gossipsub_event("slow_peer").set(0);
        Self::gossipsub_event("message_received").set(0);

        // Banned Peers
        Self::banned_peers().set(0);

        // Block validation metrics
        Self::block_validation_total().absolute(0);
        Self::block_validation_success().absolute(0);

        // Block validation failures by reason
        Self::block_validation_failed("timestamp_future").absolute(0);
        Self::block_validation_failed("timestamp_past").absolute(0);
        Self::block_validation_failed("invalid_hash").absolute(0);
        Self::block_validation_failed("invalid_signature").absolute(0);
        Self::block_validation_failed("invalid_signer").absolute(0);
        Self::block_validation_failed("too_many_blocks").absolute(0);
        Self::block_validation_failed("block_seen").absolute(0);
        Self::block_validation_failed("invalid_block").absolute(0);
        Self::block_validation_failed("parent_beacon_root").absolute(0);
        Self::block_validation_failed("blob_gas_used").absolute(0);
        Self::block_validation_failed("excess_blob_gas").absolute(0);
        Self::block_validation_failed("withdrawals_root").absolute(0);

        // Block versions
        Self::block_version("v1").absolute(0);
        Self::block_version("v2").absolute(0);
        Self::block_version("v3").absolute(0);
        Self::block_version("v4").absolute(0);
    }
}
