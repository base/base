//! Contains the hardfork configuration for the chain.

use alloc::string::{String, ToString};
use core::fmt::Display;
use core::ops::{Deref, DerefMut};

use base_alloy_chains::BaseChainConfig;

/// Hardfork configuration for Base-specific upgrades.
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct BaseHardforkConfig {
    /// `v1` sets the activation time for the Base V1 network upgrade.
    /// Active if `v1` != None && L2 block timestamp >= `Some(v1)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub v1: Option<u64>,
}

impl BaseHardforkConfig {
    /// Returns true if no Base-specific hardforks are configured.
    pub const fn is_empty(&self) -> bool {
        self.v1.is_none()
    }
}

/// Legacy (OP Stack lineage) hardfork configuration.
///
/// Contains activation timestamps for forks from Regolith through Jovian. These fields are
/// frozen now that Base follows its own specification after the Jovian hardfork.
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct LegacyHardforkConfig {
    /// `regolith_time` sets the activation time of the Regolith network-upgrade:
    /// a pre-mainnet Bedrock change that addresses findings of the Sherlock contest related to
    /// deposit attributes. "Regolith" is the loose deposited rock that sits on top of Bedrock.
    /// Active if `regolith_time` != None && L2 block timestamp >= `Some(regolith_time)`, inactive
    /// otherwise.
    pub regolith_time: Option<u64>,
    /// `canyon_time` sets the activation time of the Canyon network upgrade.
    /// Active if `canyon_time` != None && L2 block timestamp >= `Some(canyon_time)`, inactive
    /// otherwise.
    pub canyon_time: Option<u64>,
    /// `delta_time` sets the activation time of the Delta network upgrade.
    /// Active if `delta_time` != None && L2 block timestamp >= `Some(delta_time)`, inactive
    /// otherwise.
    pub delta_time: Option<u64>,
    /// `ecotone_time` sets the activation time of the Ecotone network upgrade.
    /// Active if `ecotone_time` != None && L2 block timestamp >= `Some(ecotone_time)`, inactive
    /// otherwise.
    pub ecotone_time: Option<u64>,
    /// `fjord_time` sets the activation time of the Fjord network upgrade.
    /// Active if `fjord_time` != None && L2 block timestamp >= `Some(fjord_time)`, inactive
    /// otherwise.
    pub fjord_time: Option<u64>,
    /// `granite_time` sets the activation time for the Granite network upgrade.
    /// Active if `granite_time` != None && L2 block timestamp >= `Some(granite_time)`, inactive
    /// otherwise.
    pub granite_time: Option<u64>,
    /// `holocene_time` sets the activation time for the Holocene network upgrade.
    /// Active if `holocene_time` != None && L2 block timestamp >= `Some(holocene_time)`, inactive
    /// otherwise.
    pub holocene_time: Option<u64>,
    /// `pectra_blob_schedule_time` sets the activation time for the activation of the Pectra blob
    /// fee schedule for the L1 block info transaction. This is an optional fork, only present
    /// on Base sepolia chains that observed the L1 Pectra network upgrade with `op-node`
    /// <=v1.11.1 sequencing the network.
    ///
    /// Active if `pectra_blob_schedule_time` != None && L2 block timestamp >=
    /// `Some(pectra_blob_schedule_time)`, inactive otherwise.
    pub pectra_blob_schedule_time: Option<u64>,
    /// `isthmus_time` sets the activation time for the Isthmus network upgrade.
    /// Active if `isthmus_time` != None && L2 block timestamp >= `Some(isthmus_time)`, inactive
    /// otherwise.
    pub isthmus_time: Option<u64>,
    /// `jovian_time` sets the activation time for the Jovian network upgrade.
    /// Active if `jovian_time` != None && L2 block timestamp >= `Some(jovian_time)`, inactive
    /// otherwise.
    pub jovian_time: Option<u64>,
}

/// Hardfork configuration.
///
/// Groups legacy OP Stack forks (frozen after Jovian) and Base-specific forks into separate
/// sub-structs. The serialized wire format remains flat for backwards compatibility; a private
/// [`RawHardForkConfig`] handles the bridging.
///
/// Production code that accesses legacy fork timestamps (e.g. `config.hardforks.holocene_time`)
/// compiles unchanged via the [`Deref`] impl targeting [`LegacyHardforkConfig`].
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct HardForkConfig {
    /// Legacy (OP Stack lineage) hardfork activation times, frozen after Jovian.
    pub legacy: LegacyHardforkConfig,
    /// Base-specific hardfork activation times (V1+).
    pub base: BaseHardforkConfig,
}

impl Deref for HardForkConfig {
    type Target = LegacyHardforkConfig;
    fn deref(&self) -> &Self::Target {
        &self.legacy
    }
}

impl DerefMut for HardForkConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.legacy
    }
}

impl From<LegacyHardforkConfig> for HardForkConfig {
    fn from(legacy: LegacyHardforkConfig) -> Self {
        Self { legacy, base: BaseHardforkConfig::default() }
    }
}

/// Private flat representation used for serde round-tripping.
///
/// This preserves the flat JSON/TOML field layout with `deny_unknown_fields`, bridging
/// between the flat wire format and the nested in-memory [`HardForkConfig`].
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHardForkConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    regolith_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    canyon_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ecotone_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fjord_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    granite_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    holocene_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pectra_blob_schedule_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    isthmus_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jovian_time: Option<u64>,
    #[serde(default, skip_serializing_if = "BaseHardforkConfig::is_empty")]
    base: BaseHardforkConfig,
}

#[cfg(feature = "serde")]
impl From<&HardForkConfig> for RawHardForkConfig {
    fn from(cfg: &HardForkConfig) -> Self {
        Self {
            regolith_time: cfg.legacy.regolith_time,
            canyon_time: cfg.legacy.canyon_time,
            delta_time: cfg.legacy.delta_time,
            ecotone_time: cfg.legacy.ecotone_time,
            fjord_time: cfg.legacy.fjord_time,
            granite_time: cfg.legacy.granite_time,
            holocene_time: cfg.legacy.holocene_time,
            pectra_blob_schedule_time: cfg.legacy.pectra_blob_schedule_time,
            isthmus_time: cfg.legacy.isthmus_time,
            jovian_time: cfg.legacy.jovian_time,
            base: cfg.base,
        }
    }
}

#[cfg(feature = "serde")]
impl From<RawHardForkConfig> for HardForkConfig {
    fn from(raw: RawHardForkConfig) -> Self {
        Self {
            legacy: LegacyHardforkConfig {
                regolith_time: raw.regolith_time,
                canyon_time: raw.canyon_time,
                delta_time: raw.delta_time,
                ecotone_time: raw.ecotone_time,
                fjord_time: raw.fjord_time,
                granite_time: raw.granite_time,
                holocene_time: raw.holocene_time,
                pectra_blob_schedule_time: raw.pectra_blob_schedule_time,
                isthmus_time: raw.isthmus_time,
                jovian_time: raw.jovian_time,
            },
            base: raw.base,
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for HardForkConfig {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        RawHardForkConfig::from(self).serialize(s)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for HardForkConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        RawHardForkConfig::deserialize(d).map(Into::into)
    }
}

impl Display for HardForkConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[inline(always)]
        fn fmt_time(t: Option<u64>) -> String {
            t.map(|t| t.to_string()).unwrap_or_else(|| "Not scheduled".to_string())
        }

        writeln!(f, "🍴 Scheduled Hardforks:")?;
        for (name, time) in self.iter() {
            writeln!(f, "-> {} Activation Time: {}", name, fmt_time(time))?;
        }
        Ok(())
    }
}

impl HardForkConfig {
    /// Returns an iterator of hardfork names -> their activation times (if scheduled.)
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, Option<u64>)> {
        [
            ("Regolith", self.legacy.regolith_time),
            ("Canyon", self.legacy.canyon_time),
            ("Delta", self.legacy.delta_time),
            ("Ecotone", self.legacy.ecotone_time),
            ("Fjord", self.legacy.fjord_time),
            ("Granite", self.legacy.granite_time),
            ("Holocene", self.legacy.holocene_time),
            ("Pectra Blob Schedule", self.legacy.pectra_blob_schedule_time),
            ("Isthmus", self.legacy.isthmus_time),
            ("Jovian", self.legacy.jovian_time),
            ("Base V1", self.base.v1),
        ]
        .into_iter()
    }
}

impl From<&BaseChainConfig> for HardForkConfig {
    fn from(cfg: &BaseChainConfig) -> Self {
        Self {
            legacy: LegacyHardforkConfig {
                regolith_time: Some(cfg.regolith_timestamp),
                canyon_time: Some(cfg.canyon_timestamp),
                delta_time: Some(cfg.delta_timestamp),
                ecotone_time: Some(cfg.ecotone_timestamp),
                fjord_time: Some(cfg.fjord_timestamp),
                granite_time: Some(cfg.granite_timestamp),
                holocene_time: Some(cfg.holocene_timestamp),
                pectra_blob_schedule_time: cfg.pectra_blob_schedule_timestamp,
                isthmus_time: Some(cfg.isthmus_timestamp),
                jovian_time: Some(cfg.jovian_timestamp),
            },
            base: BaseHardforkConfig { v1: cfg.base_v1_timestamp },
        }
    }
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;

    #[test]
    fn test_hardforks_deserialize_json() {
        let raw: &str = r#"
        {
            "canyon_time": 1699981200,
            "delta_time": 1703203200,
            "ecotone_time": 1708534800,
            "fjord_time": 1716998400,
            "granite_time": 1723478400,
            "holocene_time":1732633200
        }
        "#;

        let hardforks: HardForkConfig = LegacyHardforkConfig {
            regolith_time: None,
            canyon_time: Some(1699981200),
            delta_time: Some(1703203200),
            ecotone_time: Some(1708534800),
            fjord_time: Some(1716998400),
            granite_time: Some(1723478400),
            holocene_time: Some(1732633200),
            pectra_blob_schedule_time: None,
            isthmus_time: None,
            jovian_time: None,
        }
        .into();

        let deserialized: HardForkConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(hardforks, deserialized);
    }

    #[test]
    fn test_hardforks_deserialize_new_field_fail_json() {
        let raw: &str = r#"
        {
            "canyon_time": 1704992401,
            "delta_time": 1708560000,
            "ecotone_time": 1710374401,
            "fjord_time": 1720627201,
            "granite_time": 1726070401,
            "holocene_time": 1736445601,
            "new_field": 0
        }
        "#;

        let err = serde_json::from_str::<HardForkConfig>(raw).unwrap_err();
        assert_eq!(err.classify(), serde_json::error::Category::Data);
    }

    #[test]
    fn test_hardforks_deserialize_toml() {
        let raw: &str = r#"
        canyon_time =  1699981200 # Tue 14 Nov 2023 17:00:00 UTC
        delta_time =   1703203200 # Fri 22 Dec 2023 00:00:00 UTC
        ecotone_time = 1708534800 # Wed 21 Feb 2024 17:00:00 UTC
        fjord_time =   1716998400 # Wed 29 May 2024 16:00:00 UTC
        granite_time = 1723478400 # Mon Aug 12 16:00:00 UTC 2024
        holocene_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        "#;

        let hardforks: HardForkConfig = LegacyHardforkConfig {
            regolith_time: None,
            canyon_time: Some(1699981200),
            delta_time: Some(1703203200),
            ecotone_time: Some(1708534800),
            fjord_time: Some(1716998400),
            granite_time: Some(1723478400),
            holocene_time: Some(1732633200),
            pectra_blob_schedule_time: None,
            isthmus_time: None,
            jovian_time: None,
        }
        .into();

        let deserialized: HardForkConfig = toml::from_str(raw).unwrap();
        assert_eq!(hardforks, deserialized);
    }

    #[test]
    fn test_hardforks_deserialize_new_field_fail_toml() {
        let raw: &str = r#"
        canyon_time =  1699981200 # Tue 14 Nov 2023 17:00:00 UTC
        delta_time =   1703203200 # Fri 22 Dec 2023 00:00:00 UTC
        ecotone_time = 1708534800 # Wed 21 Feb 2024 17:00:00 UTC
        fjord_time =   1716998400 # Wed 29 May 2024 16:00:00 UTC
        granite_time = 1723478400 # Mon Aug 12 16:00:00 UTC 2024
        holocene_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        new_field_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        "#;
        toml::from_str::<HardForkConfig>(raw).unwrap_err();
    }

    #[test]
    fn test_hardforks_iter() {
        let hardforks = HardForkConfig {
            legacy: LegacyHardforkConfig {
                regolith_time: Some(1),
                canyon_time: Some(2),
                delta_time: Some(3),
                ecotone_time: Some(4),
                fjord_time: Some(5),
                granite_time: Some(6),
                holocene_time: Some(7),
                pectra_blob_schedule_time: Some(8),
                isthmus_time: Some(9),
                jovian_time: Some(10),
            },
            base: BaseHardforkConfig { v1: Some(11) },
        };

        let mut iter = hardforks.iter();
        assert_eq!(iter.next(), Some(("Regolith", Some(1))));
        assert_eq!(iter.next(), Some(("Canyon", Some(2))));
        assert_eq!(iter.next(), Some(("Delta", Some(3))));
        assert_eq!(iter.next(), Some(("Ecotone", Some(4))));
        assert_eq!(iter.next(), Some(("Fjord", Some(5))));
        assert_eq!(iter.next(), Some(("Granite", Some(6))));
        assert_eq!(iter.next(), Some(("Holocene", Some(7))));
        assert_eq!(iter.next(), Some(("Pectra Blob Schedule", Some(8))));
        assert_eq!(iter.next(), Some(("Isthmus", Some(9))));
        assert_eq!(iter.next(), Some(("Jovian", Some(10))));
        assert_eq!(iter.next(), Some(("Base V1", Some(11))));
        assert_eq!(iter.next(), None);
    }
}
