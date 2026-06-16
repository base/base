//! CLI Options Metrics

use std::{
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base_common_chains::BaseUpgrade;
use base_common_genesis::RollupConfig;
use tracing::warn;

use crate::{P2PArgs, bootnode::BootnodeP2PArgs};

/// Metrics to record various CLI options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliMetrics;

impl CliMetrics {
    /// The default interval for refreshing active upgrade metrics.
    pub const ACTIVE_UPGRADE_RECORDING_INTERVAL: Duration = Duration::from_secs(30);

    /// Base upgrade labels emitted by [`CliMetrics::UPGRADE_ACTIVE`].
    pub const BASE_UPGRADE_LABELS: [(BaseUpgrade, &'static str); 12] = [
        (BaseUpgrade::Bedrock, "bedrock"),
        (BaseUpgrade::Regolith, "regolith"),
        (BaseUpgrade::Canyon, "canyon"),
        (BaseUpgrade::Ecotone, "ecotone"),
        (BaseUpgrade::Fjord, "fjord"),
        (BaseUpgrade::Granite, "granite"),
        (BaseUpgrade::Holocene, "holocene"),
        (BaseUpgrade::Isthmus, "isthmus"),
        (BaseUpgrade::Jovian, "jovian"),
        (BaseUpgrade::Azul, "azul"),
        (BaseUpgrade::Beryl, "beryl"),
        (BaseUpgrade::Cobalt, "cobalt"),
    ];

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

    /// The maximum number of outbound libp2p connections that may be pending at once.
    pub const P2P_MAX_PENDING_OUTGOING: &'static str = "base_node_max_pending_outgoing";

    /// The identify peerstore size.
    pub const P2P_IDENTIFY_PEERSTORE_SIZE: &'static str = "base_node_identify_peerstore_size";

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

    /// Whether each configured Base network upgrade is active.
    pub const UPGRADE_ACTIVE: &'static str = "base_node_upgrade_active";

    /// Top-level rollup config settings.
    pub const ROLLUP_CONFIG: &'static str = "base_node_rollup_config";

    /// Whether the consensus bootnode is up.
    pub const BOOTNODE_UP: &'static str = "base_node_bootnode_up";

    /// Initializes metrics for the P2P configuration.
    pub fn init_p2p(p2p: &P2PArgs) {
        metrics::describe_gauge!(
            Self::IDENTIFIER,
            "P2P configuration settings for the Base consensus node"
        );
        metrics::gauge!(
            Self::IDENTIFIER,
            &[
                (Self::P2P_PEER_SCORING_LEVEL, p2p.scoring.to_string()),
                (Self::P2P_TOPIC_SCORING_ENABLED, p2p.topic_scoring.to_string()),
                (Self::P2P_BANNING_ENABLED, p2p.ban_enabled.to_string()),
                (Self::P2P_PEER_REDIALING, p2p.peer_redial.unwrap_or(0).to_string()),
                (Self::P2P_FLOOD_PUBLISH, p2p.gossip_flood_publish.to_string()),
                (Self::P2P_DISCOVERY_INTERVAL, p2p.discovery_interval.to_string()),
                (Self::P2P_ADVERTISE_IP, p2p.advertise_ip.unwrap_or(p2p.listen_ip).to_string()),
                (
                    Self::P2P_ADVERTISE_TCP_PORT,
                    p2p.advertise_tcp_port.map_or_else(|| "auto".to_string(), |p| p.to_string())
                ),
                (
                    Self::P2P_ADVERTISE_UDP_PORT,
                    p2p.advertise_udp_port.map_or_else(|| "auto".to_string(), |p| p.to_string())
                ),
                (Self::P2P_PEERS_LO, p2p.peers_lo.to_string()),
                (Self::P2P_PEERS_HI, p2p.peers_hi.to_string()),
                (Self::P2P_MAX_PENDING_OUTGOING, p2p.max_pending_outgoing.to_string()),
                (Self::P2P_IDENTIFY_PEERSTORE_SIZE, p2p.identify_peerstore_size.to_string()),
                (Self::P2P_GOSSIP_MESH_D, p2p.gossip_mesh_d.to_string()),
                (Self::P2P_GOSSIP_MESH_D_LO, p2p.gossip_mesh_dlo.to_string()),
                (Self::P2P_GOSSIP_MESH_D_HI, p2p.gossip_mesh_dhi.to_string()),
                (Self::P2P_GOSSIP_MESH_D_LAZY, p2p.gossip_mesh_dlazy.to_string()),
                (Self::P2P_BAN_DURATION, p2p.ban_duration.to_string()),
            ]
        )
        .set(1.0);
    }

    /// Initializes metrics for the bootnode P2P discovery configuration.
    pub fn init_bootnode_p2p(p2p: &BootnodeP2PArgs) {
        metrics::describe_gauge!(
            Self::IDENTIFIER,
            "P2P discovery configuration settings for the Base consensus bootnode"
        );
        metrics::describe_gauge!(Self::BOOTNODE_UP, "Whether the Base consensus bootnode is up");
        metrics::gauge!(
            Self::IDENTIFIER,
            &[
                (Self::P2P_DISCOVERY_INTERVAL, p2p.discovery_interval.to_string()),
                (Self::P2P_ADVERTISE_IP, p2p.advertised_ip().to_string()),
                (Self::P2P_ADVERTISE_TCP_PORT, p2p.advertised_tcp_port().to_string()),
                (Self::P2P_ADVERTISE_UDP_PORT, p2p.advertised_udp_port().to_string()),
            ]
        )
        .set(1.0);
    }

    /// Records that the bootnode finished startup.
    pub fn record_bootnode_up() {
        metrics::gauge!(Self::BOOTNODE_UP).set(1.0);
    }

    /// Initializes metrics for the rollup config.
    pub fn init_rollup_config(config: &RollupConfig) {
        metrics::describe_gauge!(Self::ROLLUP_CONFIG, "Rollup configuration settings for Base");
        metrics::describe_gauge!(
            Self::HARDFORK_ACTIVATION_TIMES,
            "Activation times for hardforks in Base"
        );
        metrics::describe_gauge!(
            Self::UPGRADE_ACTIVE,
            "Whether each Base network upgrade is active according to the node configuration"
        );

        metrics::gauge!(
            Self::ROLLUP_CONFIG,
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
            // Use `-1` as a signal that the fork is not scheduled.
            let time: f64 = activation_time.map(|t| t as f64).unwrap_or(-1f64);
            metrics::gauge!(Self::HARDFORK_ACTIVATION_TIMES, "fork" => fork_name).set(time);
        }
    }

    /// Records active upgrade gauges using the current wall-clock timestamp.
    pub fn record_active_upgrades(config: &RollupConfig) {
        Self::record_active_upgrades_at(config, Self::current_unix_timestamp());
    }

    /// Records active upgrade gauges at the given Unix timestamp.
    pub fn record_active_upgrades_at(config: &RollupConfig, timestamp: u64) {
        let active_upgrade = BaseUpgrade::from_timestamp(config, timestamp);
        for (upgrade, label) in Self::BASE_UPGRADE_LABELS {
            metrics::gauge!(Self::UPGRADE_ACTIVE, "upgrade" => label)
                .set(Self::upgrade_active_value(upgrade, active_upgrade));
        }
    }

    /// Starts a background recorder that refreshes active upgrade gauges.
    pub fn spawn_active_upgrade_recorder(config: RollupConfig, interval: Duration) {
        let interval =
            if interval.is_zero() { Self::ACTIVE_UPGRADE_RECORDING_INTERVAL } else { interval };

        Self::record_active_upgrades(&config);

        if let Err(error) =
            thread::Builder::new().name("active-upgrade-metrics".to_string()).spawn(move || {
                loop {
                    thread::sleep(interval);
                    Self::record_active_upgrades(&config);
                }
            })
        {
            warn!(error = %error, "failed to spawn active upgrade metrics recorder");
        }
    }

    /// Returns the gauge value for an upgrade relative to the currently active upgrade.
    pub const fn upgrade_active_value(upgrade: BaseUpgrade, active_upgrade: BaseUpgrade) -> f64 {
        if upgrade.idx() <= active_upgrade.idx() { 1.0 } else { 0.0 }
    }

    /// Returns the current Unix timestamp in seconds.
    pub fn current_unix_timestamp() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use base_common_chains::BaseUpgrade;
    use base_common_genesis::{HardForkConfig, HardforkConfig, RollupConfig};

    use super::CliMetrics;

    fn rollup_config(azul: Option<u64>, beryl: Option<u64>, cobalt: Option<u64>) -> RollupConfig {
        RollupConfig {
            hardforks: HardForkConfig {
                base: HardforkConfig { azul, beryl, cobalt },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn active_upgrade_values_mark_azul_active_at_activation() {
        let config = rollup_config(Some(10), Some(20), Some(30));
        let active_upgrade = BaseUpgrade::from_timestamp(&config, 10);

        assert_eq!(active_upgrade, BaseUpgrade::Azul);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Jovian, active_upgrade), 1.0);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Azul, active_upgrade), 1.0);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Beryl, active_upgrade), 0.0);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Cobalt, active_upgrade), 0.0);
    }

    #[test]
    fn active_upgrade_values_mark_prior_upgrades_active_after_beryl() {
        let config = rollup_config(Some(10), Some(20), Some(30));
        let active_upgrade = BaseUpgrade::from_timestamp(&config, 25);

        assert_eq!(active_upgrade, BaseUpgrade::Beryl);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Azul, active_upgrade), 1.0);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Beryl, active_upgrade), 1.0);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Cobalt, active_upgrade), 0.0);
    }

    #[test]
    fn active_upgrade_values_keep_unscheduled_base_upgrades_inactive() {
        let config = rollup_config(None, None, None);
        let active_upgrade = BaseUpgrade::from_timestamp(&config, u64::MAX);

        assert_eq!(active_upgrade, BaseUpgrade::Bedrock);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Bedrock, active_upgrade), 1.0);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Azul, active_upgrade), 0.0);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Beryl, active_upgrade), 0.0);
        assert_eq!(CliMetrics::upgrade_active_value(BaseUpgrade::Cobalt, active_upgrade), 0.0);
    }

    #[test]
    fn upgrade_labels_are_lowercase_and_cover_every_upgrade() {
        assert_eq!(CliMetrics::BASE_UPGRADE_LABELS.len(), 12);

        for (expected_idx, (upgrade, label)) in
            CliMetrics::BASE_UPGRADE_LABELS.into_iter().enumerate()
        {
            assert_eq!(upgrade.idx(), expected_idx);
            assert!(label.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }
}
