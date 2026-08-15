//! CLI Options Metrics

use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base_common_genesis::{BaseUpgrade, RollupConfig};
use tokio::task::JoinHandle;

use crate::{P2PArgs, bootnode::BootnodeP2PArgs};

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

    /// Upgrade activation times.
    pub const UPGRADE_ACTIVATION_TIMES: &'static str = "base_node_upgrades";

    /// Seconds until the next scheduled upgrade activation.
    pub const SECONDS_UNTIL_NEXT_UPGRADE: &'static str = "base_node_seconds_until_next_upgrade";

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
            Self::UPGRADE_ACTIVATION_TIMES,
            "Activation times for upgrades in Base"
        );
        metrics::describe_gauge!(
            Self::SECONDS_UNTIL_NEXT_UPGRADE,
            "Seconds until the next scheduled Base upgrade activation"
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

        for (upgrade_name, activation_time) in config.upgrades.iter() {
            // Use `-1` as a signal that the upgrade is not scheduled.
            let time: f64 = activation_time.map(|t| t as f64).unwrap_or(-1f64);
            metrics::gauge!(Self::UPGRADE_ACTIVATION_TIMES, "upgrade" => upgrade_name).set(time);
        }
    }

    /// Starts the periodic recorder for the next scheduled upgrade countdown metric.
    ///
    /// This must be called from an active Tokio runtime. The static rollup config metrics are
    /// initialized before the runtime exists in some CLI paths, so the dynamic countdown recorder is
    /// started separately by the async command entrypoints. The recorder owns its rollup config so
    /// it can re-query runtime-aware activation timestamps on each tick.
    pub fn spawn_upgrade_countdown_recorder(config: RollupConfig) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut observed_upgrades = BTreeSet::new();
            let mut interval = tokio::time::interval(UPGRADE_COUNTDOWN_REFRESH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;
                let now = current_unix_timestamp();
                let countdown_config = config.with_runtime_upgrade_overrides();
                Self::record_seconds_until_next_upgrade(
                    &countdown_config,
                    now,
                    &mut observed_upgrades,
                );
            }
        })
    }

    fn record_seconds_until_next_upgrade(
        config: &RollupConfig,
        now: u64,
        observed_upgrades: &mut BTreeSet<&'static str>,
    ) {
        let countdowns = seconds_until_next_upgrades(config, now);
        let current_upgrades =
            countdowns.iter().map(|(upgrade, _)| *upgrade).collect::<BTreeSet<_>>();

        for (upgrade, seconds_until_activation) in countdowns {
            observed_upgrades.insert(upgrade);
            metrics::gauge!(Self::SECONDS_UNTIL_NEXT_UPGRADE, "upgrade" => upgrade)
                .set(seconds_until_activation as f64);
        }

        let stale_upgrades =
            observed_upgrades.difference(&current_upgrades).copied().collect::<Vec<_>>();

        for upgrade in &stale_upgrades {
            metrics::gauge!(Self::SECONDS_UNTIL_NEXT_UPGRADE, "upgrade" => *upgrade)
                .set(NO_UPCOMING_UPGRADE_SECONDS);
        }
        for upgrade in stale_upgrades {
            observed_upgrades.remove(upgrade);
        }
    }
}

const UPGRADE_COUNTDOWN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const UPGRADE_ACTIVATION_GRACE_SECONDS: u64 = 15 * 60;
// Use a large reset value, not `-1`, so Datadog `<= 1d/1h/0` countdown monitors recover after the
// activation grace window instead of alerting on an unscheduled sentinel.
const NO_UPCOMING_UPGRADE_SECONDS: f64 = 4_294_967_295.0;

fn current_unix_timestamp() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn seconds_until_next_upgrades(config: &RollupConfig, now: u64) -> Vec<(&'static str, u64)> {
    let next_activation = BaseUpgrade::CONTRACT_VARIANTS
        .into_iter()
        .filter_map(|upgrade| {
            config
                .contract_upgrade_activation_timestamp(upgrade)
                .filter(|activation_time| {
                    activation_time.saturating_add(UPGRADE_ACTIVATION_GRACE_SECONDS) >= now
                })
                .map(|activation_time| (upgrade, activation_time))
        })
        .min_by_key(|(_, activation_time)| *activation_time);

    let Some((_, next_activation_time)) = next_activation else {
        return Vec::new();
    };

    BaseUpgrade::CONTRACT_VARIANTS
        .into_iter()
        .filter_map(|upgrade| {
            config
                .contract_upgrade_activation_timestamp(upgrade)
                .filter(|activation_time| *activation_time == next_activation_time)
                .and_then(|activation_time| {
                    upgrade_metric_label(upgrade)
                        .map(|label| (label, activation_time.saturating_sub(now)))
                })
        })
        .collect()
}

const UPGRADE_METRIC_LABELS: [(BaseUpgrade, &str); BaseUpgrade::CONTRACT_VARIANTS.len()] = [
    (BaseUpgrade::Regolith, "Regolith"),
    (BaseUpgrade::Canyon, "Canyon"),
    (BaseUpgrade::Delta, "Delta"),
    (BaseUpgrade::Ecotone, "Ecotone"),
    (BaseUpgrade::Fjord, "Fjord"),
    (BaseUpgrade::Granite, "Granite"),
    (BaseUpgrade::Holocene, "Holocene"),
    (BaseUpgrade::PectraBlobSchedule, "Pectra Blob Schedule"),
    (BaseUpgrade::Isthmus, "Isthmus"),
    (BaseUpgrade::Jovian, "Jovian"),
    (BaseUpgrade::Azul, "Azul"),
    (BaseUpgrade::Beryl, "Beryl"),
    (BaseUpgrade::Cobalt, "Cobalt"),
];

fn upgrade_metric_label(upgrade: BaseUpgrade) -> Option<&'static str> {
    UPGRADE_METRIC_LABELS
        .iter()
        .find_map(|(candidate, label)| (*candidate == upgrade).then_some(*label))
}

#[cfg(test)]
mod tests {
    use alloy_chains::Chain;
    use base_common_genesis::{RuntimeUpgradeRegistry, UpgradeConfig};

    use super::*;

    fn upgrade_metric_labels() -> Vec<(BaseUpgrade, &'static str)> {
        UPGRADE_METRIC_LABELS.to_vec()
    }

    #[test]
    fn seconds_until_next_upgrades_returns_future_countdown() {
        let config = RollupConfig {
            upgrades: UpgradeConfig {
                base: base_common_genesis::BaseUpgradeConfig {
                    azul: Some(1_000),
                    beryl: Some(2_000),
                    cobalt: Some(3_000),
                },
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(seconds_until_next_upgrades(&config, 800), vec![("Azul", 200)]);
    }

    #[test]
    fn seconds_until_next_upgrades_returns_zero_during_activation_grace() {
        let config = RollupConfig {
            upgrades: UpgradeConfig {
                base: base_common_genesis::BaseUpgradeConfig {
                    beryl: Some(1_000),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(seconds_until_next_upgrades(&config, 1_000), vec![("Beryl", 0)]);
        assert_eq!(seconds_until_next_upgrades(&config, 1_100), vec![("Beryl", 0)]);
    }

    #[test]
    fn seconds_until_next_upgrades_ignores_activations_after_grace() {
        let config = RollupConfig {
            upgrades: UpgradeConfig {
                base: base_common_genesis::BaseUpgradeConfig {
                    beryl: Some(1_000),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(
            seconds_until_next_upgrades(&config, 1_000 + UPGRADE_ACTIVATION_GRACE_SECONDS + 1)
                .is_empty()
        );
    }

    #[test]
    fn seconds_until_next_upgrades_returns_all_simultaneous_next_upgrades() {
        let config = RollupConfig {
            upgrades: UpgradeConfig {
                base: base_common_genesis::BaseUpgradeConfig {
                    azul: Some(1_000),
                    beryl: Some(1_000),
                    cobalt: Some(2_000),
                },
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(seconds_until_next_upgrades(&config, 900), vec![("Azul", 100), ("Beryl", 100)]);
    }

    #[test]
    fn seconds_until_next_upgrades_reflects_runtime_registry_updates() {
        let chain_id = 9_200_001;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let config = RollupConfig { l2_chain_id: Chain::from_id(chain_id), ..Default::default() };

        assert!(seconds_until_next_upgrades(&config, 900).is_empty());

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Beryl, 1_000);
        assert_eq!(seconds_until_next_upgrades(&config, 900), vec![("Beryl", 100)]);

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Beryl, 2_000);
        assert_eq!(seconds_until_next_upgrades(&config, 900), vec![("Beryl", 1_100)]);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn record_seconds_until_next_upgrade_drains_stale_upgrades() {
        let config = RollupConfig {
            upgrades: UpgradeConfig {
                base: base_common_genesis::BaseUpgradeConfig {
                    beryl: Some(1_000),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let mut observed_upgrades = BTreeSet::new();

        CliMetrics::record_seconds_until_next_upgrade(&config, 900, &mut observed_upgrades);
        assert!(observed_upgrades.contains("Beryl"));

        CliMetrics::record_seconds_until_next_upgrade(
            &config,
            1_000 + UPGRADE_ACTIVATION_GRACE_SECONDS + 1,
            &mut observed_upgrades,
        );
        assert!(observed_upgrades.is_empty());
    }

    #[test]
    fn upgrade_metric_label_matches_upgrade_activation_time_labels() {
        let labels = upgrade_metric_labels();
        assert_eq!(labels.len(), BaseUpgrade::CONTRACT_VARIANTS.len());

        let config_labels =
            UpgradeConfig::default().iter().map(|(label, _)| label).collect::<Vec<_>>();

        assert_eq!(labels.iter().map(|(_, label)| *label).collect::<Vec<_>>(), config_labels);
    }
}
