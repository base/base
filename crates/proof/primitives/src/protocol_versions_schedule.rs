use alloc::{
    collections::BTreeSet,
    string::{String, ToString},
    vec::Vec,
};

use alloy_primitives::{Address, B256, ChainId};
use base_common_genesis::{BaseUpgrade, RollupConfig};

/// Ordered `ProtocolVersions` schedule entries used to configure proof execution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProtocolVersionsSchedule {
    /// Registered upgrades in contract order.
    #[cfg_attr(feature = "serde", serde(default))]
    pub upgrades: Vec<ProtocolVersionsScheduleEntry>,
}

impl ProtocolVersionsSchedule {
    /// Returns whether the schedule contains any registered upgrades.
    pub fn is_empty(&self) -> bool {
        self.upgrades.is_empty()
    }

    /// Applies this schedule's activation overrides onto a rollup config.
    pub fn apply_to_rollup_config(
        &self,
        rollup_config: &mut RollupConfig,
    ) -> Result<(), ProtocolVersionsScheduleError> {
        let mut seen = BTreeSet::new();

        for entry in &self.upgrades {
            let upgrade = entry.upgrade()?;
            if !seen.insert(upgrade) {
                return Err(ProtocolVersionsScheduleError::DuplicateUpgrade(entry.name.clone()));
            }

            match entry.timestamp {
                Some(timestamp) => rollup_config.set_upgrade_activation_timestamp(upgrade, timestamp),
                None => rollup_config.clear_upgrade_activation_timestamp(upgrade),
            }
        }

        Ok(())
    }

    /// Computes the canonical `ProtocolVersions.scheduleId()` for this schedule.
    pub fn compute_schedule_hash(
        &self,
        rollup_config: &RollupConfig,
    ) -> Result<B256, ProtocolVersionsScheduleError> {
        Self::compute_schedule_hash_parts(
            rollup_config.l2_chain_id.id(),
            rollup_config.protocol_versions_address,
            &self.upgrades,
        )
    }

    /// Computes the canonical `ProtocolVersions.scheduleId()` from the supplied identity fields.
    pub fn compute_schedule_hash_parts(
        l2_chain_id: ChainId,
        protocol_versions_address: Address,
        upgrades: &[ProtocolVersionsScheduleEntry],
    ) -> Result<B256, ProtocolVersionsScheduleError> {
        if protocol_versions_address == Address::ZERO {
            return Err(ProtocolVersionsScheduleError::MissingProtocolVersionsAddress);
        }

        let mut seen = BTreeSet::new();
        let mut seed_input = [0u8; 64];
        seed_input[24..32].copy_from_slice(&l2_chain_id.to_be_bytes());
        seed_input[44..64].copy_from_slice(protocol_versions_address.as_slice());
        let mut schedule_hash = alloy_primitives::keccak256(seed_input);

        for entry in upgrades {
            let upgrade = entry.upgrade()?;
            if !seen.insert(upgrade) {
                return Err(ProtocolVersionsScheduleError::DuplicateUpgrade(entry.name.clone()));
            }

            let mut link_input = [0u8; 96];
            link_input[..32].copy_from_slice(schedule_hash.as_slice());
            link_input[32..64].copy_from_slice(entry.key()?.as_slice());
            link_input[88..96].copy_from_slice(&entry.timestamp.unwrap_or(0).to_be_bytes());
            schedule_hash = alloy_primitives::keccak256(link_input);
        }

        Ok(schedule_hash)
    }
}

/// One registered `ProtocolVersions` upgrade entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProtocolVersionsScheduleEntry {
    /// Contract upgrade name, such as `canyon` or `base_azul`.
    pub name: String,
    /// Activation timestamp override. `None` matches a zero / cleared timestamp on-chain.
    #[cfg_attr(feature = "serde", serde(default))]
    pub timestamp: Option<u64>,
}

impl ProtocolVersionsScheduleEntry {
    /// Resolves the canonical Base upgrade represented by this schedule entry.
    pub fn upgrade(&self) -> Result<BaseUpgrade, ProtocolVersionsScheduleError> {
        BaseUpgrade::from_contract_fork_name(&self.name)
            .ok_or_else(|| ProtocolVersionsScheduleError::UnknownUpgrade(self.name.clone()))
    }

    /// Packs the contract upgrade name into the bytes32 key used by `ProtocolVersions`.
    pub fn key(&self) -> Result<B256, ProtocolVersionsScheduleError> {
        let raw = self.name.as_bytes();
        if raw.is_empty() {
            return Err(ProtocolVersionsScheduleError::InvalidUpgradeName(self.name.clone()));
        }
        if raw.len() > 32 {
            return Err(ProtocolVersionsScheduleError::UpgradeNameTooLong(self.name.clone()));
        }

        let mut key = [0u8; 32];
        key[..raw.len()].copy_from_slice(raw);
        Ok(B256::from(key))
    }

    /// Decodes a bytes32 key emitted by `ProtocolVersions` into a schedule entry name.
    pub fn name_from_key(key: B256) -> Result<String, ProtocolVersionsScheduleError> {
        let raw = key.as_slice();
        let end = raw.iter().rposition(|byte| *byte != 0).map_or(0, |index| index + 1);
        if end == 0 {
            return Err(ProtocolVersionsScheduleError::InvalidUpgradeKey(key));
        }

        core::str::from_utf8(&raw[..end])
            .map(|name| name.to_string())
            .map_err(|_| ProtocolVersionsScheduleError::InvalidUpgradeKey(key))
    }
}

/// Schedule validation or hashing failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolVersionsScheduleError {
    /// The rollup config does not identify a `ProtocolVersions` contract.
    #[error("rollup config is missing protocol_versions_address")]
    MissingProtocolVersionsAddress,
    /// The schedule contains an unknown or unsupported contract-backed upgrade.
    #[error("unknown ProtocolVersions upgrade: {0}")]
    UnknownUpgrade(String),
    /// The schedule contains the same upgrade more than once.
    #[error("duplicate ProtocolVersions upgrade: {0}")]
    DuplicateUpgrade(String),
    /// The schedule entry name cannot be packed into the on-chain bytes32 key format.
    #[error("invalid ProtocolVersions upgrade name: {0}")]
    InvalidUpgradeName(String),
    /// The schedule entry name exceeds the on-chain 32-byte key limit.
    #[error("ProtocolVersions upgrade name exceeds 32 bytes: {0}")]
    UpgradeNameTooLong(String),
    /// The on-chain bytes32 key cannot be decoded into a UTF-8 name.
    #[error("invalid ProtocolVersions upgrade key: {0}")]
    InvalidUpgradeKey(B256),
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_chains::Chain;
    use alloy_primitives::{address, b256};

    use super::*;

    fn schedule_entry(name: &str, timestamp: Option<u64>) -> ProtocolVersionsScheduleEntry {
        ProtocolVersionsScheduleEntry { name: name.to_string(), timestamp }
    }

    fn rollup_config() -> RollupConfig {
        RollupConfig {
            l2_chain_id: Chain::from_id(8453),
            protocol_versions_address: address!("1234567890abcdef1234567890abcdef12345678"),
            ..RollupConfig::default()
        }
    }

    #[test]
    fn compute_schedule_hash_matches_protocol_versions_formula() {
        let schedule = ProtocolVersionsSchedule {
            upgrades: vec![
                schedule_entry("canyon", Some(1_111)),
                schedule_entry("ecotone", Some(2_222)),
            ],
        };

        let hash = schedule.compute_schedule_hash(&rollup_config()).unwrap();

        assert_eq!(hash, b256!("c6e7f19f404a9355652dcb265f13204b1e373dffe44fd7b5e54bb50af7a02b0e"));
    }

    #[test]
    fn apply_to_rollup_config_overrides_only_registered_upgrades() {
        let mut rollup_config = RollupConfig::default();
        rollup_config.upgrades.canyon_time = Some(10);
        rollup_config.upgrades.ecotone_time = Some(20);
        rollup_config.upgrades.fjord_time = Some(30);

        ProtocolVersionsSchedule {
            upgrades: vec![
                schedule_entry("canyon", Some(100)),
                schedule_entry("ecotone", None),
            ],
        }
        .apply_to_rollup_config(&mut rollup_config)
        .unwrap();

        assert_eq!(rollup_config.upgrades.canyon_time, Some(100));
        assert_eq!(rollup_config.upgrades.ecotone_time, None);
        assert_eq!(rollup_config.upgrades.fjord_time, Some(30));
    }

    #[test]
    fn name_from_key_round_trips_contract_key_encoding() {
        let entry = schedule_entry("base_azul", Some(123));
        let key = entry.key().unwrap();

        assert_eq!(ProtocolVersionsScheduleEntry::name_from_key(key).unwrap(), "base_azul");
    }
}
