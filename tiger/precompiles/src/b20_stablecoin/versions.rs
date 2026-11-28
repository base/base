//! Fork routing for the stablecoin B-20 precompile.

use base_common_genesis::{BaseFeature, BaseUpgrade};

use crate::B20Version;

impl B20Version {
    /// The stablecoin version active at `upgrade`, or `None` before Beryl, where the stablecoin
    /// precompile is not installed. A fork newer than Denim inherits V3 until one supersedes it.
    pub const fn stablecoin(upgrade: BaseUpgrade) -> Option<Self> {
        match upgrade {
            u if u.is_feature_enabled(BaseFeature::StablecoinPrecompileV3) => Some(Self::V3),
            u if u.is_feature_enabled(BaseFeature::StablecoinPrecompileV2) => Some(Self::V2),
            u if u.is_feature_enabled(BaseFeature::StablecoinPrecompile) => Some(Self::V1),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::{B20Abi, B20Version, };

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(B20Version::stablecoin(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(
            B20Version::stablecoin(BaseUpgrade::Beryl),
            Some(B20Version::V1)
        );
    }

    #[test]
    fn resolves_v2_at_cobalt() {
        assert_eq!(
            B20Version::stablecoin(BaseUpgrade::Cobalt),
            Some(B20Version::V2)
        );
    }

    #[test]
    fn resolves_v3_from_denim() {
        assert_eq!(
            B20Version::stablecoin(BaseUpgrade::Denim),
            Some(B20Version::V3)
        );
        assert_eq!(
            B20Version::stablecoin(BaseUpgrade::Zenith),
            Some(B20Version::V3)
        );
    }

    #[test]
    fn each_fork_resolves_to_its_wire_surface() {
        assert_eq!(B20Abi::of(B20Version::V1), B20Abi::V1);
        assert_eq!(B20Abi::of(B20Version::V2), B20Abi::V2);
        assert_eq!(B20Abi::of(B20Version::V3), B20Abi::V2);

        let beryl = B20Version::stablecoin(BaseUpgrade::Beryl).unwrap();
        let cobalt = B20Version::stablecoin(BaseUpgrade::Cobalt).unwrap();
        let denim = B20Version::stablecoin(BaseUpgrade::Denim).unwrap();
        assert_eq!(B20Abi::of(beryl), B20Abi::V1);
        assert_eq!(B20Abi::of(cobalt), B20Abi::V2);
        assert_eq!(B20Abi::of(denim), B20Abi::V2);
    }
}
