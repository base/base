use alloy_primitives::{B256, U256, keccak256};
use base_common_genesis::{BaseUpgrade, RollupConfig, UpgradeConfig};
use thiserror::Error;

/// Error returned when an upgrade schedule cannot be represented by `ProtocolVersions`.
#[derive(Debug, Clone, Copy, Eq, Error, PartialEq)]
pub enum ScheduleIdError {
    /// A contract-backed upgrade uses the local-only zero activation timestamp convention.
    #[error("Contract-backed upgrade {upgrade:?} has a zero activation timestamp")]
    ZeroActivationTimestamp {
        /// The upgrade with the invalid timestamp.
        upgrade: BaseUpgrade,
    },
}

/// Computes the locally derived schedule ID for the effective hardfork activation schedule.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct ScheduleId;

impl ScheduleId {
    /// Returns the activated schedule prefix length at `l2_timestamp`.
    pub fn activated_count(
        upgrades: &UpgradeConfig,
        l2_timestamp: u64,
    ) -> Result<usize, ScheduleIdError> {
        if let Some((upgrade, _)) = upgrades.iter().find(|(_, timestamp)| *timestamp == Some(0)) {
            return Err(ScheduleIdError::ZeroActivationTimestamp { upgrade });
        }

        Ok(BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, &upgrade)| {
                let activation_timestamp = upgrades.activation_timestamp(upgrade)?;
                (activation_timestamp <= l2_timestamp).then_some(index + 1)
            })
            .unwrap_or_default())
    }

    /// Derives the schedule ID from the first `count` registered upgrades.
    ///
    /// # Panics
    ///
    /// Panics if `count` exceeds the compiled contract-backed upgrade ladder.
    pub fn from_upgrades(upgrades: &UpgradeConfig, count: usize) -> Result<B256, ScheduleIdError> {
        assert!(
            count <= BaseUpgrade::CONTRACT_VARIANTS.len(),
            "pinned upgrade count exceeds the compiled hardfork ladder"
        );
        if let Some((upgrade, _)) = upgrades.iter().find(|(_, timestamp)| *timestamp == Some(0)) {
            return Err(ScheduleIdError::ZeroActivationTimestamp { upgrade });
        }

        let mut schedule_id = B256::ZERO;
        for (index, (_, configured_timestamp)) in upgrades.iter().enumerate().take(count) {
            let timestamp = configured_timestamp.unwrap_or_default();
            schedule_id = Self::next_link(schedule_id, index as u64, timestamp);
        }

        Ok(schedule_id)
    }

    /// Clears upgrades above the activated prefix and returns its schedule ID.
    ///
    /// Genesis-active upgrades stored with the local-only zero timestamp convention are first
    /// normalized to the config's genesis timestamp, matching the contract schedule
    /// representation. Boot loading rejects a zero genesis timestamp before reaching this, so
    /// the [`ScheduleIdError::ZeroActivationTimestamp`] rejection is defense-in-depth for
    /// callers passing an unvalidated config.
    pub fn pin(config: &mut RollupConfig, l2_timestamp: u64) -> Result<B256, ScheduleIdError> {
        for &upgrade in &BaseUpgrade::CONTRACT_VARIANTS {
            if config.upgrades.activation_timestamp(upgrade) == Some(0) {
                config.upgrades.set_activation_timestamp(upgrade, config.genesis.l2_time);
            }
        }

        let pinned_count = Self::activated_count(&config.upgrades, l2_timestamp)?;
        for &upgrade in &BaseUpgrade::CONTRACT_VARIANTS[pinned_count..] {
            config.upgrades.clear_activation_timestamp(upgrade);
        }

        Self::from_upgrades(&config.upgrades, pinned_count)
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
        let upgrades = UpgradeConfig {
            regolith_time: Some(10),
            canyon_time: Some(20),
            base: BaseUpgradeConfig { azul: Some(30), beryl: None, cobalt: None, zenith: None },
            ..Default::default()
        };

        assert_eq!(
            ScheduleId::from_upgrades(&upgrades, 11).expect("valid schedule"),
            b256!("c04583a182a2a57476d1ca57f9e5e7aa24f1eee188221f75fe17634b8a63f00c")
        );
    }

    #[test]
    fn schedule_id_is_zero_when_nothing_is_active() {
        let upgrades =
            UpgradeConfig { regolith_time: None, canyon_time: Some(101), ..Default::default() };

        assert_eq!(ScheduleId::activated_count(&upgrades, 99).expect("valid schedule"), 0);
        assert_eq!(ScheduleId::from_upgrades(&upgrades, 0).expect("valid schedule"), B256::ZERO);
    }

    #[test]
    fn schedule_id_uses_inclusive_activation_boundary() {
        let upgrades = UpgradeConfig { regolith_time: Some(10), ..Default::default() };

        assert_eq!(ScheduleId::activated_count(&upgrades, 9).expect("valid schedule"), 0);
        assert_eq!(ScheduleId::activated_count(&upgrades, 10).expect("valid schedule"), 1);
        assert_eq!(
            ScheduleId::from_upgrades(&upgrades, 1).expect("valid schedule"),
            ScheduleId::next_link(B256::ZERO, 0, 10)
        );
    }

    #[test]
    fn schedule_id_commits_inactive_gap_below_active_upgrade() {
        let upgrades = UpgradeConfig {
            regolith_time: Some(10),
            canyon_time: Some(50),
            delta_time: Some(30),
            ..Default::default()
        };

        let expected = ScheduleId::next_link(
            ScheduleId::next_link(ScheduleId::next_link(B256::ZERO, 0, 10), 1, 50),
            2,
            30,
        );
        assert_eq!(ScheduleId::activated_count(&upgrades, 30).expect("valid schedule"), 3);
        assert_eq!(ScheduleId::from_upgrades(&upgrades, 3).expect("valid schedule"), expected);
    }

    #[test]
    fn schedule_id_rejects_zero_activation_timestamps() {
        for upgrade in BaseUpgrade::CONTRACT_VARIANTS {
            let mut upgrades = UpgradeConfig::default();
            upgrades.set_activation_timestamp(upgrade, 0);
            let expected = ScheduleIdError::ZeroActivationTimestamp { upgrade };

            assert_eq!(ScheduleId::activated_count(&upgrades, 100).unwrap_err(), expected);
            assert_eq!(ScheduleId::from_upgrades(&upgrades, 1).unwrap_err(), expected);

            // A zero genesis timestamp cannot normalize the zero convention away.
            let mut config = RollupConfig { upgrades, ..Default::default() };
            assert_eq!(ScheduleId::pin(&mut config, 100).unwrap_err(), expected);
        }
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

        let schedule_id = ScheduleId::pin(&mut config, 60).expect("valid schedule");

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

        let schedule_id = ScheduleId::pin(&mut config, 30).expect("valid schedule");

        assert_eq!(config.upgrades.regolith_time, Some(10));
        assert_eq!(config.upgrades.canyon_time, Some(50));
        assert_eq!(config.upgrades.delta_time, Some(30));
        assert_eq!(config.upgrades.base.azul, None);
        assert_eq!(
            schedule_id,
            ScheduleId::from_upgrades(&config.upgrades, 3).expect("valid schedule")
        );
    }

    #[test]
    #[should_panic(expected = "pinned upgrade count exceeds the compiled hardfork ladder")]
    fn from_upgrades_rejects_count_beyond_ladder() {
        let _ = ScheduleId::from_upgrades(
            &UpgradeConfig::default(),
            BaseUpgrade::CONTRACT_VARIANTS.len() + 1,
        );
    }
}
