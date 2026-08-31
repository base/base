//! Base precompile set selection.

use base_common_genesis::BaseUpgrade;
use evm2::{Precompiles, precompiles::P256VERIFY};

use crate::{BaseEvmTypes, BaseSpecId};

impl BaseEvmTypes {
    /// Returns the Base precompile set for `spec`.
    ///
    /// Starts from the stock Ethereum set for the mapped EVM2 spec ([`Precompiles::base`]) and
    /// layers the Base-specific differences on top. Base enables RIP-7212 `P256VERIFY`
    /// (secp256r1, `0x100`) from **Fjord**, ahead of its upstream Osaka introduction; from
    /// **Azul** (which maps to Osaka) the Osaka-priced `P256VERIFY` is already installed by
    /// [`Precompiles::base`], so the earlier-priced variant is only bridged for Fjord..<Azul.
    ///
    /// The Base-specific bn254 pairing input caps (Granite/Jovian), the Isthmus/Jovian BLS12-381
    /// variants, and the Beryl/Cobalt dynamic precompiles (B20 factory, registries, `TxContext`,
    /// `NonceManager`) are not yet ported and remain follow-up work.
    pub fn precompiles(spec: BaseSpecId) -> Precompiles<Self> {
        let upgrade = spec.upgrade();
        let mut precompiles = Precompiles::base(spec.into());
        if (upgrade as u8) >= (BaseUpgrade::Fjord as u8)
            && (upgrade as u8) < (BaseUpgrade::Azul as u8)
        {
            precompiles.as_map_mut().insert(P256VERIFY());
        }
        precompiles
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    /// The RIP-7212 secp256r1 `P256VERIFY` precompile address.
    const P256_ADDRESS: alloy_primitives::Address =
        address!("0x0000000000000000000000000000000000000100");

    #[test]
    fn p256_absent_before_fjord() {
        let mut precompiles = BaseEvmTypes::precompiles(BaseSpecId::new(BaseUpgrade::Ecotone));
        assert!(!precompiles.as_map_mut().contains(&P256_ADDRESS), "no P256 before Fjord");
    }

    #[test]
    fn p256_present_from_fjord() {
        for upgrade in [BaseUpgrade::Fjord, BaseUpgrade::Granite, BaseUpgrade::Isthmus] {
            let mut precompiles = BaseEvmTypes::precompiles(BaseSpecId::new(upgrade));
            assert!(
                precompiles.as_map_mut().contains(&P256_ADDRESS),
                "P256 present at {upgrade:?}",
            );
        }
    }

    #[test]
    fn p256_present_at_azul_via_osaka() {
        // Azul maps to Osaka, where `Precompiles::base` already installs the Osaka-priced P256.
        let mut precompiles = BaseEvmTypes::precompiles(BaseSpecId::new(BaseUpgrade::Azul));
        assert!(precompiles.as_map_mut().contains(&P256_ADDRESS), "P256 present at Azul");
    }
}
