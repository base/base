use alloy_primitives::{B256, U256, keccak256};
use base_common_genesis::{RollupConfig, UpgradeConfig};

/// Computes the locally derived schedule ID for the effective hardfork activation schedule.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct ScheduleId;

impl ScheduleId {
    /// Derives the schedule ID from the hardfork timestamps in a rollup config.
    pub fn from_rollup_config(rollup_config: &RollupConfig) -> B256 {
        Self::from_upgrades(&rollup_config.upgrades)
    }

    /// Derive the schedule ID from upgrade timestamps in canonical field order.
    ///
    /// This mirrors the onchain `ProtocolVersions.scheduleId()` hash chain:
    /// `link[i + 1] = keccak256(abi.encode(link[i], i, timestamp_i))`.
    pub fn from_upgrades(upgrades: &UpgradeConfig) -> B256 {
        let mut schedule_id = B256::ZERO;
        for (index, (_, timestamp)) in upgrades.iter().enumerate() {
            schedule_id = Self::next_link(schedule_id, index as u64, timestamp.unwrap_or_default());
        }

        schedule_id
    }

    fn next_link(previous: B256, index: u64, timestamp: u64) -> B256 {
        let mut buf = [0u8; 96];
        buf[..32].copy_from_slice(previous.as_slice());
        buf[32..64].copy_from_slice(&U256::from(index).to_be_bytes::<32>());
        buf[64..].copy_from_slice(&U256::from(timestamp).to_be_bytes::<32>());
        keccak256(buf)
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};

    use super::*;

    #[test]
    fn schedule_id_changes_when_schedule_changes() {
        let a = UpgradeConfig {
            regolith_time: Some(1),
            canyon_time: Some(2),
            base: BaseUpgradeConfig { azul: Some(3), beryl: None, cobalt: None },
            ..Default::default()
        };
        let b = UpgradeConfig {
            regolith_time: Some(1),
            canyon_time: Some(4),
            base: BaseUpgradeConfig { azul: Some(3), beryl: None, cobalt: None },
            ..Default::default()
        };

        assert_ne!(ScheduleId::from_upgrades(&a), ScheduleId::from_upgrades(&b));
    }
}
