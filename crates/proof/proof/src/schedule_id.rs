use alloy_primitives::{B256, U256, keccak256};
use base_common_genesis::{BaseUpgrade, UpgradeConfig};

/// Computes the locally derived schedule ID for the effective hardfork activation schedule.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct ScheduleId;

impl ScheduleId {
    /// Returns the number of schedule links pinned at `l2_timestamp`.
    ///
    /// Searches the registration ladder from newest to oldest and returns the highest active
    /// upgrade index plus one. This mirrors `ProtocolVersions.activatedScheduleId(uint64)`, which
    /// returns the cached cumulative link through that upgrade.
    #[must_use]
    pub fn activated_count(
        upgrades: &UpgradeConfig,
        l2_timestamp: u64,
        genesis_timestamp: u64,
    ) -> usize {
        BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, &upgrade)| {
                let configured_timestamp = upgrades.activation_timestamp(upgrade)?;
                let activation_timestamp = if configured_timestamp == 0 {
                    genesis_timestamp
                } else {
                    configured_timestamp
                };
                (activation_timestamp != 0 && activation_timestamp <= l2_timestamp)
                    .then_some(index + 1)
            })
            .unwrap_or_default()
    }

    /// Derives the schedule ID from the first `count` upgrades in registration order.
    ///
    /// Every registered entry in the prefix contributes a link, including unscheduled zero-valued
    /// entries and future timestamps below the highest active upgrade. Locally configured zero
    /// timestamps mean active at genesis and are committed as `genesis_timestamp`, matching the
    /// positive timestamp used when seeding the contract's historical schedule.
    ///
    /// # Panics
    ///
    /// Panics if `count` exceeds the compiled contract-backed upgrade ladder.
    #[must_use]
    pub fn from_upgrades(upgrades: &UpgradeConfig, count: usize, genesis_timestamp: u64) -> B256 {
        assert!(
            count <= BaseUpgrade::CONTRACT_VARIANTS.len(),
            "pinned upgrade count exceeds the compiled hardfork ladder"
        );

        let mut schedule_id = B256::ZERO;
        for (index, (_, configured_timestamp)) in upgrades.iter().enumerate().take(count) {
            let timestamp = match configured_timestamp {
                Some(0) => genesis_timestamp,
                Some(timestamp) => timestamp,
                None => 0,
            };
            schedule_id = Self::next_link(schedule_id, index as u64, timestamp);
        }

        schedule_id
    }

    /// Removes entries above the highest active upgrade and returns the pinned schedule ID.
    ///
    /// Entries within the prefix remain in the derivation config because their timestamps are
    /// committed by the returned hash, even when they are unscheduled or activate after the
    /// supplied L2 timestamp.
    pub fn pin(upgrades: &mut UpgradeConfig, l2_timestamp: u64, genesis_timestamp: u64) -> B256 {
        let pinned_count = Self::activated_count(upgrades, l2_timestamp, genesis_timestamp);
        for &upgrade in &BaseUpgrade::CONTRACT_VARIANTS[pinned_count..] {
            upgrades.clear_activation_timestamp(upgrade);
        }

        Self::from_upgrades(upgrades, pinned_count, genesis_timestamp)
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
            ScheduleId::from_upgrades(&upgrades, 11, 1),
            b256!("c04583a182a2a57476d1ca57f9e5e7aa24f1eee188221f75fe17634b8a63f00c")
        );
    }

    #[test]
    fn schedule_id_is_zero_when_nothing_is_active() {
        let upgrades =
            UpgradeConfig { regolith_time: Some(0), canyon_time: Some(101), ..Default::default() };

        assert_eq!(ScheduleId::activated_count(&upgrades, 99, 100), 0);
        assert_eq!(ScheduleId::from_upgrades(&upgrades, 0, 100), B256::ZERO);
    }

    #[test]
    fn schedule_id_uses_inclusive_activation_boundary() {
        let upgrades = UpgradeConfig { regolith_time: Some(10), ..Default::default() };

        assert_eq!(ScheduleId::activated_count(&upgrades, 9, 1), 0);
        assert_eq!(ScheduleId::activated_count(&upgrades, 10, 1), 1);
        assert_eq!(
            ScheduleId::from_upgrades(&upgrades, 1, 1),
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
        assert_eq!(ScheduleId::activated_count(&upgrades, 30, 1), 3);
        assert_eq!(ScheduleId::from_upgrades(&upgrades, 3, 1), expected);
    }

    #[test]
    fn schedule_id_normalizes_genesis_active_timestamp() {
        let upgrades = UpgradeConfig { regolith_time: Some(0), ..Default::default() };

        assert_eq!(ScheduleId::activated_count(&upgrades, 100, 100), 1);
        assert_eq!(
            ScheduleId::from_upgrades(&upgrades, 1, 100),
            ScheduleId::next_link(B256::ZERO, 0, 100)
        );
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

        let schedule_id = ScheduleId::pin(&mut upgrades, 30, 1);

        assert_eq!(upgrades.regolith_time, Some(10));
        assert_eq!(upgrades.canyon_time, Some(50));
        assert_eq!(upgrades.delta_time, Some(30));
        assert_eq!(upgrades.base.azul, None);
        assert_eq!(schedule_id, ScheduleId::from_upgrades(&upgrades, 3, 1));
    }

    #[test]
    fn pin_clears_unrepresentable_zero_timestamp() {
        let mut upgrades = UpgradeConfig { regolith_time: Some(0), ..Default::default() };

        assert_eq!(ScheduleId::pin(&mut upgrades, 0, 0), B256::ZERO);
        assert_eq!(upgrades.regolith_time, None);
    }

    #[test]
    #[should_panic(expected = "pinned upgrade count exceeds the compiled hardfork ladder")]
    fn from_upgrades_rejects_count_beyond_ladder() {
        let _ = ScheduleId::from_upgrades(
            &UpgradeConfig::default(),
            BaseUpgrade::CONTRACT_VARIANTS.len() + 1,
            1,
        );
    }
}
