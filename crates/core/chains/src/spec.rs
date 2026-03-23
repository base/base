//! Base EVM spec ID type.

use core::str::FromStr;

#[cfg(feature = "revm")]
use revm::primitives::hardfork::SpecId;

/// Error indicating an unknown Base hardfork name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownOpHardfork;

/// Base spec id.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[allow(non_camel_case_types)]
pub enum OpSpecId {
    /// Bedrock spec id.
    BEDROCK = 100,
    /// Regolith spec id.
    REGOLITH,
    /// Canyon spec id.
    CANYON,
    /// Ecotone spec id.
    ECOTONE,
    /// Fjord spec id.
    FJORD,
    /// Granite spec id.
    GRANITE,
    /// Holocene spec id.
    HOLOCENE,
    /// Isthmus spec id.
    #[default]
    ISTHMUS,
    /// Jovian spec id.
    JOVIAN,
    /// Base V1 spec id.
    BASE_V1,
}

impl OpSpecId {
    /// Checks if the [`OpSpecId`] is enabled in the other [`OpSpecId`].
    pub const fn is_enabled_in(self, other: Self) -> bool {
        other as u8 <= self as u8
    }

    /// Converts the [`OpSpecId`] into a [`revm::primitives::hardfork::SpecId`].
    #[cfg(feature = "revm")]
    pub const fn into_eth_spec(self) -> SpecId {
        match self {
            Self::BEDROCK | Self::REGOLITH => SpecId::MERGE,
            Self::CANYON => SpecId::SHANGHAI,
            Self::ECOTONE | Self::FJORD | Self::GRANITE | Self::HOLOCENE => SpecId::CANCUN,
            Self::ISTHMUS | Self::JOVIAN => SpecId::PRAGUE,
            Self::BASE_V1 => SpecId::OSAKA,
        }
    }
}

impl FromStr for OpSpecId {
    type Err = UnknownOpHardfork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            BEDROCK => Ok(Self::BEDROCK),
            REGOLITH => Ok(Self::REGOLITH),
            CANYON => Ok(Self::CANYON),
            ECOTONE => Ok(Self::ECOTONE),
            FJORD => Ok(Self::FJORD),
            GRANITE => Ok(Self::GRANITE),
            HOLOCENE => Ok(Self::HOLOCENE),
            ISTHMUS => Ok(Self::ISTHMUS),
            JOVIAN => Ok(Self::JOVIAN),
            BASE_V1 => Ok(Self::BASE_V1),
            _ => Err(UnknownOpHardfork),
        }
    }
}

impl From<OpSpecId> for &'static str {
    fn from(spec_id: OpSpecId) -> Self {
        match spec_id {
            OpSpecId::BEDROCK => BEDROCK,
            OpSpecId::REGOLITH => REGOLITH,
            OpSpecId::CANYON => CANYON,
            OpSpecId::ECOTONE => ECOTONE,
            OpSpecId::FJORD => FJORD,
            OpSpecId::GRANITE => GRANITE,
            OpSpecId::HOLOCENE => HOLOCENE,
            OpSpecId::ISTHMUS => ISTHMUS,
            OpSpecId::JOVIAN => JOVIAN,
            OpSpecId::BASE_V1 => BASE_V1,
        }
    }
}

#[cfg(feature = "revm")]
impl From<OpSpecId> for SpecId {
    fn from(spec: OpSpecId) -> Self {
        spec.into_eth_spec()
    }
}

const BEDROCK: &str = "Bedrock";
const REGOLITH: &str = "Regolith";
const CANYON: &str = "Canyon";
const ECOTONE: &str = "Ecotone";
const FJORD: &str = "Fjord";
const GRANITE: &str = "Granite";
const HOLOCENE: &str = "Holocene";
const ISTHMUS: &str = "Isthmus";
const JOVIAN: &str = "Jovian";
const BASE_V1: &str = "V1";
