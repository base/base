//! Base fork schedule for the EVM2 execution type family.

use base_common_genesis::BaseUpgrade;
use evm2::SpecId;

/// A Base fork identifier for EVM2 execution.
///
/// Wraps a Base network [`BaseUpgrade`] and maps it — via [`From<BaseSpecId>`] for [`SpecId`] —
/// to the EVM2 spec whose feature set and gas schedule govern execution at that upgrade. Used
/// as [`BaseEvmTypes::SpecId`](crate::BaseEvmTypes), it keeps the active upgrade recoverable at
/// runtime (`Evm::config_spec_id`) for fork-dependent fee logic, even when several upgrades
/// share one EVM2 spec (e.g. Ecotone and Fjord both map to [`SpecId::CANCUN`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaseSpecId(BaseUpgrade);

impl BaseSpecId {
    /// Wraps a Base network upgrade.
    pub const fn new(upgrade: BaseUpgrade) -> Self {
        Self(upgrade)
    }

    /// Returns the wrapped Base network upgrade.
    pub const fn upgrade(self) -> BaseUpgrade {
        self.0
    }
}

impl From<BaseUpgrade> for BaseSpecId {
    fn from(upgrade: BaseUpgrade) -> Self {
        Self(upgrade)
    }
}

impl From<BaseSpecId> for SpecId {
    /// Maps a Base upgrade to the EVM2 spec that governs execution at that upgrade.
    ///
    /// Mirrors the revm-side mapping (`base-common-chains`' `BaseUpgrade::into_eth_spec`),
    /// duplicated here rather than reused because that mapping targets revm's `SpecId` and
    /// pulls in revm, which this crate deliberately avoids.
    fn from(spec: BaseSpecId) -> Self {
        match spec.0 {
            BaseUpgrade::Bedrock | BaseUpgrade::Regolith => Self::MERGE,
            BaseUpgrade::Canyon | BaseUpgrade::Delta => Self::SHANGHAI,
            BaseUpgrade::Ecotone
            | BaseUpgrade::Fjord
            | BaseUpgrade::Granite
            | BaseUpgrade::Holocene
            | BaseUpgrade::PectraBlobSchedule => Self::CANCUN,
            BaseUpgrade::Isthmus | BaseUpgrade::Jovian => Self::PRAGUE,
            // Azul, Beryl, Cobalt, Denim, Zenith, and newer upgrades inherit the latest known
            // Ethereum spec until explicitly mapped.
            _ => Self::OSAKA,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_wrapped_upgrade() {
        let spec = BaseSpecId::new(BaseUpgrade::Fjord);
        assert_eq!(spec.upgrade(), BaseUpgrade::Fjord);
        assert_eq!(BaseSpecId::from(BaseUpgrade::Fjord), spec);
    }
}
