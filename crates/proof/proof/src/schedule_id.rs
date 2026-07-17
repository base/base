use alloy_primitives::{B256, U256, keccak256};
use base_common_genesis::UpgradeConfig;

/// Computes the locally derived schedule ID for the effective hardfork activation schedule.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct ScheduleId;

impl ScheduleId {
    /// Derive the schedule ID from upgrade timestamps in canonical field order.
    ///
    /// This mirrors the onchain `ProtocolVersions.scheduleId()` hash chain:
    /// `link[i + 1] = keccak256(abi.encode(link[i], i, timestamp_i))`.
    pub fn from_upgrades(upgrades: &UpgradeConfig) -> B256 {
        let mut schedule_id = B256::ZERO;
        for (index, (_, timestamp)) in upgrades.iter().enumerate() {
            // unwrap_or_default() maps None (unscheduled) to 0, matching the onchain
            // ProtocolVersions.scheduleId() which uses 0 for unscheduled entries.
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
    use alloy_primitives::b256;
    use base_common_genesis::{BaseUpgradeConfig, UpgradeConfig};

    use super::*;

    #[test]
    fn schedule_id_matches_golden_values() {
        // Pin the hash against encoding/order drift; MUST equal the onchain
        // `ProtocolVersions.scheduleId()` for the same schedule.

        // All unscheduled: still 13 links of `(index, 0)`, a fixed non-zero hash.
        assert_eq!(
            ScheduleId::from_upgrades(&UpgradeConfig::default()),
            b256!("c61ddfdfe1ff9422919909549df660a43d53127a318e01274a0448443e54146d")
        );

        // A representative partial schedule.
        let upgrades = UpgradeConfig {
            regolith_time: Some(10),
            canyon_time: Some(20),
            base: BaseUpgradeConfig { azul: Some(30), beryl: None, cobalt: None },
            ..Default::default()
        };
        assert_eq!(
            ScheduleId::from_upgrades(&upgrades),
            b256!("8701f0422d18caf7dbd5d2d00321a16c44e9dfa706dd4b0b4ea3c66fbe776f42")
        );
    }

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
