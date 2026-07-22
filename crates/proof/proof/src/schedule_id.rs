use alloy_primitives::{B256, U256, keccak256};
use base_common_genesis::UpgradeConfig;

/// Computes the locally derived schedule ID for the effective hardfork activation schedule.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct ScheduleId;

impl ScheduleId {
    /// Derive the schedule ID from upgrade timestamps in canonical field order.
    ///
    /// This mirrors the onchain `ProtocolVersions.scheduleId()` hash chain, which links only
    /// *scheduled* upgrades: `link[i + 1] = keccak256(abi.encode(link[i], i, timestamp_i))` when
    /// `timestamp_i != 0`, and `link[i + 1] = link[i]` otherwise.
    pub fn from_upgrades(upgrades: &UpgradeConfig) -> B256 {
        let mut schedule_id = B256::ZERO;
        for (index, (_, timestamp)) in upgrades.iter().enumerate() {
            match timestamp {
                // Unscheduled (None, or 0 = the contract's "not scheduled" sentinel): carry the
                // link forward.
                None | Some(0) => {}
                Some(timestamp) => {
                    schedule_id = Self::next_link(schedule_id, index as u64, timestamp);
                }
            }
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

        // All unscheduled: no entry contributes a link, so the ID is the bytes32(0) seed.
        assert_eq!(ScheduleId::from_upgrades(&UpgradeConfig::default()), B256::ZERO);

        // A representative partial schedule. Only the scheduled entries (regolith at contract
        // index 0, canyon at 1, azul at 10) contribute links; the unscheduled gap between canyon
        // and azul is skipped. Independently derived via
        // `cast keccak (cast abi-encode "f(bytes32,uint256,uint256)" <prev> <index> <timestamp>)`.
        let upgrades = UpgradeConfig {
            regolith_time: Some(10),
            canyon_time: Some(20),
            base: BaseUpgradeConfig { azul: Some(30), beryl: None, cobalt: None },
            ..Default::default()
        };
        assert_eq!(
            ScheduleId::from_upgrades(&upgrades),
            b256!("a24ace1024856cce0f999daface9269cbe7c1a8d1069a0e75196635146ee2058")
        );
    }

    #[test]
    fn schedule_id_matches_mainnet_golden_value() {
        // Cross-implementation golden value for the Base mainnet schedule as of Beryl, shared
        // with the contracts repo (`test_scheduleId_matchesMainnetGoldenValue_succeeds` in
        // test/L1/ProtocolVersions.t.sol). Genesis-active regolith is pinned to the genesis
        // timestamp (0 is the contract's "not scheduled" sentinel); the mainnet
        // pectra_blob_schedule gap (None) and the unscheduled cobalt tail contribute no link.
        let mainnet = UpgradeConfig {
            regolith_time: Some(1_686_789_347),
            canyon_time: Some(1_704_992_401),
            delta_time: Some(1_708_560_000),
            ecotone_time: Some(1_710_374_401),
            fjord_time: Some(1_720_627_201),
            granite_time: Some(1_726_070_401),
            holocene_time: Some(1_736_445_601),
            pectra_blob_schedule_time: None,
            isthmus_time: Some(1_746_806_401),
            jovian_time: Some(1_764_691_201),
            base: BaseUpgradeConfig {
                azul: Some(1_779_991_200),
                beryl: Some(1_782_410_400),
                cobalt: None,
            },
        };

        assert_eq!(
            ScheduleId::from_upgrades(&mainnet),
            b256!("689503a0192dda23fbb770faf397d562a78ff4ec69df10b596c94c9a437e0f72")
        );
    }

    #[test]
    fn schedule_id_treats_zero_timestamp_as_unscheduled() {
        // The contract encodes "not scheduled" as timestamp 0, so `Some(0)` must hash
        // identically to `None`.
        let zero =
            UpgradeConfig { regolith_time: Some(0), canyon_time: Some(20), ..Default::default() };
        let none =
            UpgradeConfig { regolith_time: None, canyon_time: Some(20), ..Default::default() };

        assert_eq!(ScheduleId::from_upgrades(&zero), ScheduleId::from_upgrades(&none));
    }

    #[test]
    fn schedule_id_unchanged_when_unscheduled_upgrade_is_registered() {
        // The async-registration property: a schedule that differs only in unscheduled entries
        // (here beryl/cobalt None, mirroring a contract that registered them with timestamp 0)
        // produces the same ID, so contract and prover upgrade lists can grow independently.
        let scheduled_only =
            UpgradeConfig { regolith_time: Some(10), canyon_time: Some(20), ..Default::default() };
        let with_unscheduled_tail = UpgradeConfig {
            regolith_time: Some(10),
            canyon_time: Some(20),
            base: BaseUpgradeConfig { azul: None, beryl: None, cobalt: None },
            ..Default::default()
        };

        assert_eq!(
            ScheduleId::from_upgrades(&scheduled_only),
            ScheduleId::from_upgrades(&with_unscheduled_tail)
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
