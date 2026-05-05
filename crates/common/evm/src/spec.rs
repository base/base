//! Contains the `[BaseSpecId]` type and its implementation.

use alloy_consensus::BlockHeader;
use base_common_chains::Upgrades;
use revm::primitives::hardfork::SpecId;

/// Base spec id.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    strum::Display,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[allow(non_camel_case_types)]
pub enum BaseSpecId {
    /// Bedrock spec id.
    #[strum(serialize = "Bedrock")]
    BEDROCK = 100,
    /// Regolith spec id.
    #[strum(serialize = "Regolith")]
    REGOLITH,
    /// Canyon spec id.
    #[strum(serialize = "Canyon")]
    CANYON,
    /// Ecotone spec id.
    #[strum(serialize = "Ecotone")]
    ECOTONE,
    /// Fjord spec id.
    #[strum(serialize = "Fjord")]
    FJORD,
    /// Granite spec id.
    #[strum(serialize = "Granite")]
    GRANITE,
    /// Holocene spec id.
    #[strum(serialize = "Holocene")]
    HOLOCENE,
    /// Isthmus spec id.
    #[default]
    #[strum(serialize = "Isthmus")]
    ISTHMUS,
    /// Jovian spec id.
    #[strum(serialize = "Jovian")]
    JOVIAN,
    /// Base Azul spec id.
    #[strum(serialize = "Azul")]
    AZUL,
    /// Beryl spec id.
    #[strum(serialize = "Beryl")]
    BERYL,
}

impl BaseSpecId {
    /// Converts the [`BaseSpecId`] into a [`SpecId`].
    pub const fn into_eth_spec(self) -> SpecId {
        match self {
            Self::BEDROCK | Self::REGOLITH => SpecId::MERGE,
            Self::CANYON => SpecId::SHANGHAI,
            Self::ECOTONE | Self::FJORD | Self::GRANITE | Self::HOLOCENE => SpecId::CANCUN,
            Self::ISTHMUS | Self::JOVIAN => SpecId::PRAGUE,
            Self::AZUL | Self::BERYL => SpecId::OSAKA,
        }
    }

    /// Checks if the [`BaseSpecId`] is enabled in the other [`BaseSpecId`].
    pub const fn is_enabled_in(self, other: Self) -> bool {
        other as u8 <= self as u8
    }

    /// Parses the [`BaseSpecId`] from the chain spec and block header.
    pub fn from_header(chain_spec: impl Upgrades, header: impl BlockHeader) -> Self {
        Self::from_timestamp(chain_spec, header.timestamp())
    }

    /// Returns the [`BaseSpecId`] at the given timestamp.
    ///
    /// # Note
    ///
    /// This is only intended to be used after the Bedrock, when hardforks are activated by
    /// timestamp.
    pub fn from_timestamp(chain_spec: impl Upgrades, timestamp: u64) -> Self {
        if chain_spec.is_beryl_active_at_timestamp(timestamp) {
            Self::BERYL
        } else if chain_spec.is_base_azul_active_at_timestamp(timestamp) {
            Self::AZUL
        } else if chain_spec.is_jovian_active_at_timestamp(timestamp) {
            Self::JOVIAN
        } else if chain_spec.is_isthmus_active_at_timestamp(timestamp) {
            Self::ISTHMUS
        } else if chain_spec.is_holocene_active_at_timestamp(timestamp) {
            Self::HOLOCENE
        } else if chain_spec.is_granite_active_at_timestamp(timestamp) {
            Self::GRANITE
        } else if chain_spec.is_fjord_active_at_timestamp(timestamp) {
            Self::FJORD
        } else if chain_spec.is_ecotone_active_at_timestamp(timestamp) {
            Self::ECOTONE
        } else if chain_spec.is_canyon_active_at_timestamp(timestamp) {
            Self::CANYON
        } else if chain_spec.is_regolith_active_at_timestamp(timestamp) {
            Self::REGOLITH
        } else {
            Self::BEDROCK
        }
    }
}

impl From<BaseSpecId> for SpecId {
    fn from(spec: BaseSpecId) -> Self {
        spec.into_eth_spec()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn test_base_spec_id_eth_spec_compatibility() {
        // Define test cases: (BaseSpecId, enabled in ETH specs, enabled in Base specs)
        let test_cases = [
            (
                BaseSpecId::BEDROCK,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, false),
                    (SpecId::CANCUN, false),
                    (SpecId::default(), false),
                ],
                vec![(BaseSpecId::BEDROCK, true), (BaseSpecId::REGOLITH, false)],
            ),
            (
                BaseSpecId::REGOLITH,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, false),
                    (SpecId::CANCUN, false),
                    (SpecId::default(), false),
                ],
                vec![(BaseSpecId::BEDROCK, true), (BaseSpecId::REGOLITH, true)],
            ),
            (
                BaseSpecId::CANYON,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, false),
                    (SpecId::default(), false),
                ],
                vec![
                    (BaseSpecId::BEDROCK, true),
                    (BaseSpecId::REGOLITH, true),
                    (BaseSpecId::CANYON, true),
                ],
            ),
            (
                BaseSpecId::ECOTONE,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::default(), false),
                ],
                vec![
                    (BaseSpecId::BEDROCK, true),
                    (BaseSpecId::REGOLITH, true),
                    (BaseSpecId::CANYON, true),
                    (BaseSpecId::ECOTONE, true),
                ],
            ),
            (
                BaseSpecId::FJORD,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::default(), false),
                ],
                vec![
                    (BaseSpecId::BEDROCK, true),
                    (BaseSpecId::REGOLITH, true),
                    (BaseSpecId::CANYON, true),
                    (BaseSpecId::ECOTONE, true),
                    (BaseSpecId::FJORD, true),
                ],
            ),
            (
                BaseSpecId::JOVIAN,
                vec![
                    (SpecId::PRAGUE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::MERGE, true),
                ],
                vec![
                    (BaseSpecId::BEDROCK, true),
                    (BaseSpecId::REGOLITH, true),
                    (BaseSpecId::CANYON, true),
                    (BaseSpecId::ECOTONE, true),
                    (BaseSpecId::FJORD, true),
                    (BaseSpecId::HOLOCENE, true),
                    (BaseSpecId::ISTHMUS, true),
                ],
            ),
            (
                BaseSpecId::AZUL,
                vec![
                    (SpecId::OSAKA, true),
                    (SpecId::PRAGUE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::MERGE, true),
                ],
                vec![
                    (BaseSpecId::BEDROCK, true),
                    (BaseSpecId::REGOLITH, true),
                    (BaseSpecId::CANYON, true),
                    (BaseSpecId::ECOTONE, true),
                    (BaseSpecId::FJORD, true),
                    (BaseSpecId::HOLOCENE, true),
                    (BaseSpecId::ISTHMUS, true),
                    (BaseSpecId::JOVIAN, true),
                ],
            ),
            (
                BaseSpecId::BERYL,
                vec![
                    (SpecId::OSAKA, true),
                    (SpecId::PRAGUE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::MERGE, true),
                ],
                vec![
                    (BaseSpecId::BEDROCK, true),
                    (BaseSpecId::REGOLITH, true),
                    (BaseSpecId::CANYON, true),
                    (BaseSpecId::ECOTONE, true),
                    (BaseSpecId::FJORD, true),
                    (BaseSpecId::HOLOCENE, true),
                    (BaseSpecId::ISTHMUS, true),
                    (BaseSpecId::JOVIAN, true),
                    (BaseSpecId::AZUL, true),
                ],
            ),
        ];

        for (base_spec, eth_tests, base_tests) in test_cases {
            // Test ETH spec compatibility
            for (eth_spec, expected) in eth_tests {
                assert_eq!(
                    base_spec.into_eth_spec().is_enabled_in(eth_spec),
                    expected,
                    "{:?} should {} be enabled in ETH {:?}",
                    base_spec,
                    if expected { "" } else { "not " },
                    eth_spec
                );
            }

            // Test Base spec compatibility
            for (other_base_spec, expected) in base_tests {
                assert_eq!(
                    base_spec.is_enabled_in(other_base_spec),
                    expected,
                    "{:?} should {} be enabled in Base {:?}",
                    base_spec,
                    if expected { "" } else { "not " },
                    other_base_spec
                );
            }
        }
    }

    #[test]
    fn default_base_spec_id() {
        assert_eq!(BaseSpecId::default(), BaseSpecId::ISTHMUS);
    }
}
