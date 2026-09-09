use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, U256, address};
use base_common_genesis::{BaseUpgrade, RollupConfig};
use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Inputs supported by the fixed offline development network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisConfig {
    /// L1 chain ID.
    pub l1_chain_id: u64,
    /// L2 chain ID.
    pub l2_chain_id: u64,
    /// Beacon slot duration in seconds.
    pub slot_duration: u64,
    /// Deployer, sequencer, batcher, proposer, and challenger role addresses.
    pub roles: [Address; 5],
    /// Base activation administrator; defaults to the sequencer.
    pub activation_admin: Address,
    /// Explicit Base/Isthmus activation block numbers.
    pub upgrades: BTreeMap<String, u64>,
    /// Fixed genesis time for reproducible tests; otherwise the current Unix time.
    pub timestamp: Option<u64>,
    /// Fixed deployment salt for reproducible tests; otherwise random.
    pub salt: Option<B256>,
    /// Write runtime upgrade-signal settings for the real `ProtocolVersions` registry.
    pub preinstall_upgrade_signal: bool,
    /// Initial minimum protocol version (uint128) exposed by `ProtocolVersions`.
    pub minimum_version: U256,
}

impl Default for GenesisConfig {
    fn default() -> Self {
        let roles = [
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            address!("9965507D1a55bcC2695C58ba16FB37d819B0A4dc"),
            address!("976EA74026E726554dB657fA54763abd0C3a0aa9"),
            address!("14dC79964da2C08b23698B3D3cc7Ca32193d9955"),
            address!("23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f"),
        ];
        Self {
            l1_chain_id: 1337,
            l2_chain_id: 84538453,
            slot_duration: 12,
            activation_admin: roles[1],
            roles,
            upgrades: BTreeMap::new(),
            timestamp: None,
            salt: None,
            preinstall_upgrade_signal: true,
            minimum_version: U256::from(4294967296u64),
        }
    }
}

impl GenesisConfig {
    /// Initial L2 minimum base fee, shared by the contract, rollup, and genesis header.
    pub const MIN_BASE_FEE: u64 = 1_000_000_000;

    /// Rejects invalid or unrepresentable configurations before doing deployment work.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.l1_chain_id != 0 && self.l2_chain_id != 0, "chain IDs must be positive");
        ensure!(self.slot_duration > 0, "slot duration must be positive");
        ensure!(
            !self.roles.contains(&Address::ZERO) && self.activation_admin != Address::ZERO,
            "role addresses must be nonzero"
        );
        ensure!(self.salt != Some(B256::ZERO), "explicit deployment salt must be nonzero");
        ensure!(
            self.minimum_version <= U256::from(u128::MAX),
            "minimum protocol version exceeds uint128"
        );
        for name in self.upgrades.keys() {
            ensure!(
                ["isthmus", "azul", "beryl", "cobalt", "denim", "zenith"].contains(&name.as_str()),
                "unsupported upgrade {name}"
            );
        }
        Ok(())
    }

    /// Checks registry inputs against the notice period read from the pinned contract.
    ///
    /// Deployment runs at timestamp zero. Zero entries are unscheduled and do not
    /// participate in ordering; equal nonzero activations are permitted.
    pub fn validate_schedule(&self, schedule: &[u64], minimum_notice: u64) -> Result<()> {
        self.validate()?;
        ensure!(
            schedule.len() == BaseUpgrade::CONTRACT_VARIANTS.len(),
            "ProtocolVersions schedule has an unexpected number of upgrades"
        );
        let mut previous = None;
        for (upgrade, &timestamp) in BaseUpgrade::CONTRACT_VARIANTS.iter().zip(schedule) {
            if timestamp == 0 {
                continue;
            }
            ensure!(
                !self.minimum_version.is_zero(),
                "minimum protocol version must be nonzero when upgrades are scheduled"
            );
            ensure!(
                timestamp >= minimum_notice,
                "{} activation at {timestamp} requires at least {minimum_notice} seconds of notice from bootstrap timestamp zero",
                upgrade.contract_id()
            );
            if let Some((previous_name, previous_time)) = previous {
                ensure!(
                    timestamp >= previous_time,
                    "{} activation at {timestamp} precedes {previous_name} at {previous_time}",
                    upgrade.contract_id()
                );
            }
            previous = Some((upgrade.contract_id(), timestamp));
        }
        Ok(())
    }

    /// Applies the actual Cobalt cadence with checked arithmetic and whole-second gates.
    ///
    /// Jovian follows an explicit Isthmus override because it has no independent devnet setting.
    pub fn apply_upgrades(&self, rollup: &mut RollupConfig) -> Result<()> {
        let genesis = rollup.genesis.l2_time;
        for upgrade in [
            BaseUpgrade::Isthmus,
            BaseUpgrade::Azul,
            BaseUpgrade::Beryl,
            BaseUpgrade::Cobalt,
            BaseUpgrade::Denim,
            BaseUpgrade::Zenith,
        ] {
            let name = upgrade.contract_id();
            let Some(block) = self.upgrades.get(name) else { continue };
            let cobalt = self.upgrades.get("cobalt").copied().unwrap_or(*block);
            let millis = u128::from(genesis) * 1000
                + u128::from((*block).min(cobalt)) * u128::from(rollup.block_time) * 1000
                + u128::from(block.saturating_sub(cobalt))
                    * u128::from(RollupConfig::NATIVE_SUBSECOND_BLOCK_INTERVAL_MILLIS);
            ensure!(millis <= u128::from(u64::MAX), "{name} timestamp overflow");
            // Genesis generation must not consult a running node's process-global
            // runtime upgrade overrides. Use the same cadence, with checked inputs.
            let seconds = (millis / 1000) as u64;
            ensure!(millis % 1000 == 0, "{name} must align to a whole second after Cobalt");
            rollup.set_upgrade_activation_timestamp(upgrade, seconds);
            if upgrade == BaseUpgrade::Isthmus {
                rollup.set_upgrade_activation_timestamp(BaseUpgrade::Jovian, seconds);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_validation_handles_holes_equal_times_and_unscheduled_upgrades() -> Result<()> {
        let config = GenesisConfig::default();
        let mut schedule = vec![4000; BaseUpgrade::CONTRACT_VARIANTS.len()];
        schedule[7] = 0;
        config.validate_schedule(&schedule, 3600)?;
        schedule[10] = 4500;
        let error = config.validate_schedule(&schedule, 3600).unwrap_err();
        assert!(error.to_string().contains("precedes azul"));
        schedule[10] = 4000;
        schedule[0] = 3599;
        assert!(
            config.validate_schedule(&schedule, 3600).unwrap_err().to_string().contains("notice")
        );
        let config = GenesisConfig { minimum_version: U256::ZERO, ..config };
        assert!(
            config.validate_schedule(&schedule, 0).unwrap_err().to_string().contains("nonzero")
        );
        config.validate_schedule(&vec![0; BaseUpgrade::CONTRACT_VARIANTS.len()], 3600)?;
        Ok(())
    }

    #[test]
    fn jovian_follows_explicit_isthmus_activation() -> Result<()> {
        for (block, expected) in [(0, 1000), (10, 1020)] {
            let config = GenesisConfig {
                upgrades: [("isthmus".into(), block)].into(),
                ..Default::default()
            };
            let mut rollup = RollupConfig { block_time: 2, ..Default::default() };
            rollup.genesis.l2_time = 1000;
            rollup.upgrades.jovian_time = Some(0);
            config.apply_upgrades(&mut rollup)?;
            assert_eq!(rollup.upgrades.isthmus_time, Some(expected));
            assert_eq!(rollup.upgrades.jovian_time, Some(expected));
        }
        Ok(())
    }

    #[test]
    fn upgrade_blocks_follow_cobalt_not_denim() -> Result<()> {
        for (cobalt, denim, zenith, expected) in [
            (Some(22), 27, 102, (1045, 1060)),
            (None, 25, 100, (1050, 1200)),
            (Some(0), 5, 10, (1001, 1002)),
            (Some(5), 10, 10, (1011, 1011)),
        ] {
            let mut config = GenesisConfig::default();
            config.upgrades.insert("denim".into(), denim);
            config.upgrades.insert("zenith".into(), zenith);
            if let Some(cobalt) = cobalt {
                config.upgrades.insert("cobalt".into(), cobalt);
            }
            let mut rollup = RollupConfig { block_time: 2, ..Default::default() };
            rollup.genesis.l2_time = 1000;
            config.apply_upgrades(&mut rollup)?;
            assert_eq!(rollup.upgrades.base.denim, Some(expected.0));
            assert_eq!(rollup.upgrades.base.zenith, Some(expected.1));
            assert_eq!(rollup.l2_block_timestamp(denim), expected.0);
            assert_eq!(rollup.l2_block_timestamp(zenith), expected.1);
        }
        Ok(())
    }

    #[test]
    fn rejects_unaligned_and_overflowing_activations() {
        let mut config = GenesisConfig::default();
        config.upgrades.extend([("cobalt".into(), 22), ("denim".into(), 25)]);
        let mut rollup = RollupConfig { block_time: 2, ..Default::default() };
        assert!(
            config.apply_upgrades(&mut rollup).unwrap_err().to_string().contains("whole second")
        );
        config.upgrades.insert("cobalt".into(), u64::MAX);
        assert!(config.apply_upgrades(&mut rollup).unwrap_err().to_string().contains("overflow"));
    }
}
