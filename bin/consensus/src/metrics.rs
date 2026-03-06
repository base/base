//! CLI Options Metrics

use base_client_cli::P2PArgs;
use base_consensus_genesis::RollupConfig;

/// Metrics to record various CLI options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliMetrics;

impl CliMetrics {
    /// The identifier for the cli metrics gauge.
    pub const IDENTIFIER: &'static str = "base_cli_opts";

    /// The P2P Scoring level (disabled if "off").
    pub const P2P_PEER_SCORING_LEVEL: &'static str = "base_node_peer_scoring_level";

    /// Whether P2P Topic Scoring is enabled.
    pub const P2P_TOPIC_SCORING_ENABLED: &'static str = "base_node_topic_scoring_enabled";

    /// Whether P2P banning is enabled.
    pub const P2P_BANNING_ENABLED: &'static str = "base_node_banning_enabled";

    /// The value for peer redialing.
    pub const P2P_PEER_REDIALING: &'static str = "base_node_peer_redialing";

    /// Whether flood publishing is enabled.
    pub const P2P_FLOOD_PUBLISH: &'static str = "base_node_flood_publish";

    /// The interval to send FINDNODE requests through discv5.
    pub const P2P_DISCOVERY_INTERVAL: &'static str = "base_node_discovery_interval";

    /// The IP to advertise via P2P.
    pub const P2P_ADVERTISE_IP: &'static str = "base_node_advertise_ip";

    /// The advertised tcp port via P2P.
    pub const P2P_ADVERTISE_TCP_PORT: &'static str = "base_node_advertise_tcp";

    /// The advertised udp port via P2P.
    pub const P2P_ADVERTISE_UDP_PORT: &'static str = "base_node_advertise_udp";

    /// The low-tide peer count.
    pub const P2P_PEERS_LO: &'static str = "base_node_peers_lo";

    /// The high-tide peer count.
    pub const P2P_PEERS_HI: &'static str = "base_node_peers_hi";

    /// The gossip mesh d option.
    pub const P2P_GOSSIP_MESH_D: &'static str = "base_node_gossip_mesh_d";

    /// The gossip mesh d lo option.
    pub const P2P_GOSSIP_MESH_D_LO: &'static str = "base_node_gossip_mesh_d_lo";

    /// The gossip mesh d hi option.
    pub const P2P_GOSSIP_MESH_D_HI: &'static str = "base_node_gossip_mesh_d_hi";

    /// The gossip mesh d lazy option.
    pub const P2P_GOSSIP_MESH_D_LAZY: &'static str = "base_node_gossip_mesh_d_lazy";

    /// The duration to ban peers.
    pub const P2P_BAN_DURATION: &'static str = "base_node_ban_duration";

    /// Hardfork activation times.
    pub const HARDFORK_ACTIVATION_TIMES: &'static str = "base_node_hardforks";

    /// Top-level rollup config settings.
    pub const ROLLUP_CONFIG: &'static str = "base_node_rollup_config";
}

/// Initializes metrics for the P2P configuration.
pub fn init_p2p_metrics(p2p: &P2PArgs) {
    metrics::describe_gauge!(
        CliMetrics::IDENTIFIER,
        "P2P configuration settings for the Base consensus node"
    );
    metrics::gauge!(
        CliMetrics::IDENTIFIER,
        &[
            (CliMetrics::P2P_PEER_SCORING_LEVEL, p2p.scoring.to_string()),
            (CliMetrics::P2P_TOPIC_SCORING_ENABLED, p2p.topic_scoring.to_string()),
            (CliMetrics::P2P_BANNING_ENABLED, p2p.ban_enabled.to_string()),
            (CliMetrics::P2P_PEER_REDIALING, p2p.peer_redial.unwrap_or(0).to_string()),
            (CliMetrics::P2P_FLOOD_PUBLISH, p2p.gossip_flood_publish.to_string()),
            (CliMetrics::P2P_DISCOVERY_INTERVAL, p2p.discovery_interval.to_string()),
            (CliMetrics::P2P_ADVERTISE_IP, p2p.advertise_ip.unwrap_or(p2p.listen_ip).to_string()),
            (
                CliMetrics::P2P_ADVERTISE_TCP_PORT,
                p2p.advertise_tcp_port.map_or_else(|| "auto".to_string(), |p| p.to_string())
            ),
            (
                CliMetrics::P2P_ADVERTISE_UDP_PORT,
                p2p.advertise_udp_port.map_or_else(|| "auto".to_string(), |p| p.to_string())
            ),
            (CliMetrics::P2P_PEERS_LO, p2p.peers_lo.to_string()),
            (CliMetrics::P2P_PEERS_HI, p2p.peers_hi.to_string()),
            (CliMetrics::P2P_GOSSIP_MESH_D, p2p.gossip_mesh_d.to_string()),
            (CliMetrics::P2P_GOSSIP_MESH_D_LO, p2p.gossip_mesh_dlo.to_string()),
            (CliMetrics::P2P_GOSSIP_MESH_D_HI, p2p.gossip_mesh_dhi.to_string()),
            (CliMetrics::P2P_GOSSIP_MESH_D_LAZY, p2p.gossip_mesh_dlazy.to_string()),
            (CliMetrics::P2P_BAN_DURATION, p2p.ban_duration.to_string()),
        ]
    )
    .set(1.0);
}

/// Initializes metrics for the rollup config.
pub fn init_rollup_config_metrics(config: &RollupConfig) {
    metrics::describe_gauge!(
        CliMetrics::ROLLUP_CONFIG,
        "Rollup configuration settings for Base"
    );
    metrics::describe_gauge!(
        CliMetrics::HARDFORK_ACTIVATION_TIMES,
        "Activation times for hardforks in Base"
    );

    metrics::gauge!(
        CliMetrics::ROLLUP_CONFIG,
        &[
            ("l1_genesis_block_num", config.genesis.l1.number.to_string()),
            ("l2_genesis_block_num", config.genesis.l2.number.to_string()),
            ("genesis_l2_time", config.genesis.l2_time.to_string()),
            ("l1_chain_id", config.l1_chain_id.to_string()),
            ("l2_chain_id", config.l2_chain_id.to_string()),
            ("block_time", config.block_time.to_string()),
            ("max_sequencer_drift", config.max_sequencer_drift.to_string()),
            ("sequencer_window_size", config.seq_window_size.to_string()),
            ("channel_timeout", config.channel_timeout.to_string()),
            ("granite_channel_timeout", config.granite_channel_timeout.to_string()),
            ("batch_inbox_address", config.batch_inbox_address.to_string()),
            ("deposit_contract_address", config.deposit_contract_address.to_string()),
            ("l1_system_config_address", config.l1_system_config_address.to_string()),
            ("protocol_versions_address", config.protocol_versions_address.to_string()),
        ]
    )
    .set(1);

    for (fork_name, activation_time) in config.hardforks.iter() {
        // Set the value of the metric for the given hardfork, using `-1` as a signal that the
        // fork is not scheduled.
        let time: f64 = activation_time.map(|t| t as f64).unwrap_or(-1f64);
        metrics::gauge!(CliMetrics::HARDFORK_ACTIVATION_TIMES, "fork" => fork_name).set(time);
    }
}
