use alloy_primitives::{B256, U256, keccak256};
use base_common_genesis::{BaseUpgrade, RollupConfig};

/// Computes the locally derived schedule ID for the effective hardfork activation schedule.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct ScheduleId;

impl ScheduleId {
    /// Clears upgrades above the prefix activated at `l2_timestamp` and returns its schedule ID.
    ///
    /// This mirrors the onchain `ProtocolVersions.scheduleId()` hash chain:
    /// `link[i + 1] = keccak256(abi.encode(link[i], i, timestamp_i))`, committing the activated
    /// schedule prefix in contract registration order with 0 for unscheduled entries.
    ///
    /// Genesis-active upgrades using the legacy zero timestamp convention are first normalized to
    /// the config's genesis timestamp, matching the contract schedule representation. Boot loading
    /// rejects a zero genesis timestamp before reaching this, so a zero timestamp cannot survive
    /// normalization.
    pub fn pin(config: &mut RollupConfig, l2_timestamp: u64) -> B256 {
        for &upgrade in &BaseUpgrade::CONTRACT_VARIANTS {
            if config.upgrades.activation_timestamp(upgrade) == Some(0) {
                config.upgrades.set_activation_timestamp(upgrade, config.genesis.l2_time);
            }
        }

        let pinned_count = BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, &upgrade)| {
                let activation_timestamp = config.upgrades.activation_timestamp(upgrade)?;
                (activation_timestamp <= l2_timestamp).then_some(index + 1)
            })
            .unwrap_or_default();
        for &upgrade in &BaseUpgrade::CONTRACT_VARIANTS[pinned_count..] {
            config.upgrades.clear_activation_timestamp(upgrade);
        }

        let mut schedule_id = B256::ZERO;
        for (index, &upgrade) in
            BaseUpgrade::CONTRACT_VARIANTS.iter().enumerate().take(pinned_count)
        {
            // unwrap_or_default() maps None (unscheduled) to 0, matching the onchain
            // ProtocolVersions.scheduleId() which uses 0 for unscheduled entries.
            let timestamp = config.upgrades.activation_timestamp(upgrade).unwrap_or_default();
            schedule_id = Self::next_link(schedule_id, index as u64, timestamp);
        }

        schedule_id
    }

    /// Extends the schedule hash chain with one registered upgrade.
    #[must_use]
    pub fn next_link(previous: B256, index: u64, timestamp: u64) -> B256 {
        let mut buf = [0u8; 96];
        buf[..32].copy_from_slice(previous.as_slice());
        buf[32..64].copy_from_slice(&U256::from(index).to_be_bytes::<32>());
        buf[64..].copy_from_slice(&U256::from(timestamp).to_be_bytes::<32>());
        keccak256(buf)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;
    use base_common_genesis::{BaseUpgradeConfig, ChainGenesis, UpgradeConfig};

    use super::*;

    #[test]
    fn schedule_id_matches_contract_golden_value() {
        // ProtocolVersions schedule: [10, 20, 0, ..., 0, 30].
        let mut config = RollupConfig {
            upgrades: UpgradeConfig {
                regolith_time: Some(10),
                canyon_time: Some(20),
                base: BaseUpgradeConfig { azul: Some(30), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            ScheduleId::pin(&mut config, 30),
            b256!("c04583a182a2a57476d1ca57f9e5e7aa24f1eee188221f75fe17634b8a63f00c")
        );
    }

    #[test]
    fn schedule_id_is_zero_when_nothing_is_active() {
        let mut config = RollupConfig {
            upgrades: UpgradeConfig {
                regolith_time: None,
                canyon_time: Some(101),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(ScheduleId::pin(&mut config, 99), B256::ZERO);
        assert_eq!(config.upgrades.canyon_time, None);
    }

    #[test]
    fn schedule_id_uses_inclusive_activation_boundary() {
        let mut config = RollupConfig {
            upgrades: UpgradeConfig { regolith_time: Some(10), ..Default::default() },
            ..Default::default()
        };

        assert_eq!(ScheduleId::pin(&mut config.clone(), 9), B256::ZERO);
        assert_eq!(ScheduleId::pin(&mut config, 10), ScheduleId::next_link(B256::ZERO, 0, 10));
    }

    #[test]
    fn schedule_id_commits_inactive_gap_below_active_upgrade() {
        let mut config = RollupConfig {
            upgrades: UpgradeConfig {
                regolith_time: Some(10),
                canyon_time: Some(50),
                delta_time: Some(30),
                ..Default::default()
            },
            ..Default::default()
        };

        let expected = ScheduleId::next_link(
            ScheduleId::next_link(ScheduleId::next_link(B256::ZERO, 0, 10), 1, 50),
            2,
            30,
        );
        assert_eq!(ScheduleId::pin(&mut config, 30), expected);
    }

    #[test]
    fn pin_normalizes_genesis_active_zero_timestamps() {
        let mut config = RollupConfig {
            genesis: ChainGenesis { l2_time: 10, ..Default::default() },
            upgrades: UpgradeConfig {
                regolith_time: Some(0),
                canyon_time: Some(50),
                ..Default::default()
            },
            ..Default::default()
        };

        let schedule_id = ScheduleId::pin(&mut config, 60);

        assert_eq!(config.upgrades.regolith_time, Some(10));
        assert_eq!(
            schedule_id,
            ScheduleId::next_link(ScheduleId::next_link(B256::ZERO, 0, 10), 1, 50)
        );
    }

    #[test]
    fn pin_clears_only_entries_above_activated_prefix() {
        let mut config = RollupConfig {
            upgrades: UpgradeConfig {
                regolith_time: Some(10),
                canyon_time: Some(50),
                delta_time: Some(30),
                base: BaseUpgradeConfig { azul: Some(1_000), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };

        let schedule_id = ScheduleId::pin(&mut config, 30);

        assert_eq!(config.upgrades.regolith_time, Some(10));
        assert_eq!(config.upgrades.canyon_time, Some(50));
        assert_eq!(config.upgrades.delta_time, Some(30));
        assert_eq!(config.upgrades.base.azul, None);
        assert_eq!(
            schedule_id,
            ScheduleId::next_link(
                ScheduleId::next_link(ScheduleId::next_link(B256::ZERO, 0, 10), 1, 50),
                2,
                30,
            )
        );
    }
}
