use alloy_primitives::{B256, U256, keccak256};
use base_common_genesis::{BaseUpgrade, UpgradeConfig};
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
    /// Returns the number of schedule links pinned at `l2_timestamp`.
    ///
    /// Searches the registration ladder from newest to oldest and returns the highest active
    /// upgrade index plus one. This mirrors `ProtocolVersions.activatedScheduleId(uint64)`, which
    /// returns the cached cumulative link through that upgrade.
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

    /// Derives the schedule ID from the first `count` upgrades in registration order.
    ///
    /// Every registered entry in the prefix contributes a link, including unscheduled zero-valued
    /// entries and future timestamps below the highest active upgrade.
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

    /// Removes entries above the highest active upgrade and returns the pinned schedule ID.
    ///
    /// Entries within the prefix remain in the derivation config because their timestamps are
    /// committed by the returned hash, even when they are unscheduled or activate after the
    /// supplied L2 timestamp.
    pub fn pin(upgrades: &mut UpgradeConfig, l2_timestamp: u64) -> Result<B256, ScheduleIdError> {
        let pinned_count = Self::activated_count(upgrades, l2_timestamp)?;
        for &upgrade in &BaseUpgrade::CONTRACT_VARIANTS[pinned_count..] {
            upgrades.clear_activation_timestamp(upgrade);
        }

        Self::from_upgrades(upgrades, pinned_count)
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
    use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};

    use super::*;

    #[test]
    fn schedule_id_matches_contract_golden_value() {
        // Cross-implementation golden shared with ProtocolVersions: ids 0 and 1 activate at 10
        // and 20, ids 2 through 9 are unscheduled, and id 10 activates at 30. The commitment
        // contains the complete prefix through id 10.
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
            assert_eq!(ScheduleId::pin(&mut upgrades, 100).unwrap_err(), expected);
        }
    }

    #[test]
    fn pin_clears_only_entries_above_activated_prefix() {
        let mut upgrades = UpgradeConfig {
            regolith_time: Some(10),
            canyon_time: Some(50),
            delta_time: Some(30),
            base: BaseUpgradeConfig { azul: Some(1_000), ..Default::default() },
            ..Default::default()
        };

        let schedule_id = ScheduleId::pin(&mut upgrades, 30).expect("valid schedule");

        assert_eq!(upgrades.regolith_time, Some(10));
        assert_eq!(upgrades.canyon_time, Some(50));
        assert_eq!(upgrades.delta_time, Some(30));
        assert_eq!(upgrades.base.azul, None);
        assert_eq!(schedule_id, ScheduleId::from_upgrades(&upgrades, 3).expect("valid schedule"));
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
