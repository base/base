use alloy_hardforks::ForkCondition;
use base_common_chain_config::ChainUpgrades;

/// Displays the Base upgrade schedule without derived Ethereum duplicates.
#[derive(Debug)]
pub struct UpgradeDisplay(pub ChainUpgrades);

impl core::fmt::Display for UpgradeDisplay {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (upgrade, condition) in self.0.iter() {
            if condition != ForkCondition::Never {
                writeln!(f, "{upgrade}: {condition:?}")?;
            }
        }
        Ok(())
    }
}
