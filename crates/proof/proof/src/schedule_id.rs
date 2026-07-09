use alloy_primitives::{B256, keccak256};
use base_common_genesis::{HardForkConfig, RollupConfig};

/// Computes the locally derived schedule ID for the effective hardfork activation schedule.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct ScheduleId;

impl ScheduleId {
    /// Derive the schedule ID from the hardfork timestamps embedded in a rollup config.
    pub fn from_rollup_config(rollup_config: &RollupConfig) -> B256 {
        Self::from_hardforks(&rollup_config.hardforks)
    }

    /// Derive the schedule ID from hardfork timestamps in canonical field order.
    ///
    /// This mirrors the onchain `ProtocolVersions.scheduleId()` hash chain:
    /// `link[i + 1] = keccak256(abi.encode(link[i], i, timestamp_i))`.
    pub fn from_hardforks(hardforks: &HardForkConfig) -> B256 {
        let mut schedule_id = B256::ZERO;
        for (index, (_, timestamp)) in hardforks.iter().enumerate() {
            schedule_id = Self::next_link(schedule_id, index as u64, timestamp.unwrap_or_default());
        }

        schedule_id
    }

    fn next_link(previous: B256, index: u64, timestamp: u64) -> B256 {
        let mut buf = [0u8; 96];
        buf[..32].copy_from_slice(previous.as_slice());
        buf[32..64].copy_from_slice(&Self::encode_u256(index));
        buf[64..].copy_from_slice(&Self::encode_u256(timestamp));
        keccak256(buf)
    }

    fn encode_u256(value: u64) -> [u8; 32] {
        let mut encoded = [0u8; 32];
        encoded[24..].copy_from_slice(&value.to_be_bytes());
        encoded
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::{HardForkConfig, HardforkConfig};

    use super::*;

    #[test]
    // TODO(base): Replace this with a contract-derived golden value.
    // This rebuilds the expected hash chain from the same local field-ordering assumptions, so it
    // only checks the local implementation shape, not true compatibility with the onchain
    // scheduleId calculation.
    fn schedule_id_matches_contract_hash_chain() {
        let hardforks =
            HardForkConfig { regolith_time: Some(1), canyon_time: Some(2), ..Default::default() };

        let expected =
            hardforks.iter().enumerate().fold(B256::ZERO, |previous, (index, (_, timestamp))| {
                let mut buf = [0u8; 96];
                buf[..32].copy_from_slice(previous.as_slice());
                buf[32..64].copy_from_slice(&ScheduleId::encode_u256(index as u64));
                buf[64..].copy_from_slice(&ScheduleId::encode_u256(timestamp.unwrap_or_default()));
                keccak256(buf)
            });

        assert_eq!(ScheduleId::from_hardforks(&hardforks), expected);
    }

    #[test]
    fn schedule_id_changes_when_schedule_changes() {
        let a = HardForkConfig {
            regolith_time: Some(1),
            canyon_time: Some(2),
            base: HardforkConfig { azul: Some(3), beryl: None, cobalt: None },
            ..Default::default()
        };
        let b = HardForkConfig {
            regolith_time: Some(1),
            canyon_time: Some(4),
            base: HardforkConfig { azul: Some(3), beryl: None, cobalt: None },
            ..Default::default()
        };

        assert_ne!(ScheduleId::from_hardforks(&a), ScheduleId::from_hardforks(&b));
    }
}
