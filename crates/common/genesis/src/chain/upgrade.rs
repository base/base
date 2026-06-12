//! Contains the upgrade configuration for the chain.

use alloc::string::{String, ToString};
use core::fmt::Display;

/// Upgrade configuration for Base-specific upgrades.
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct BaseUpgradeConfig {
    /// `azul` sets the activation time for the Base Azul network upgrade.
    /// Active if `azul` != None && L2 block timestamp >= `Some(azul)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(alias = "v1", skip_serializing_if = "Option::is_none"))]
    pub azul: Option<u64>,
    /// `beryl` sets the activation time for the Beryl network upgrade.
    /// Active if `beryl` != None && L2 block timestamp >= `Some(beryl)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(alias = "v2", skip_serializing_if = "Option::is_none"))]
    pub beryl: Option<u64>,
    /// `cobalt` sets the activation time for the Cobalt network upgrade.
    /// Active if `cobalt` != None && L2 block timestamp >= `Some(cobalt)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(alias = "v3", skip_serializing_if = "Option::is_none"))]
    pub cobalt: Option<u64>,
}

impl BaseUpgradeConfig {
    /// Returns true if no Base-specific upgrades are configured.
    pub const fn is_empty(&self) -> bool {
        self.azul.is_none() && self.beryl.is_none() && self.cobalt.is_none()
    }
}

/// Upgrade configuration.
///
/// See: <https://github.com/ethereum-optimism/superchain-registry/blob/8ff62ada16e14dd59d0fb94ffb47761c7fa96e01/ops/internal/config/chain.go#L102-L110>
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct UpgradeConfig {
    /// `regolith_time` sets the activation time of the Regolith network-upgrade:
    /// a pre-mainnet Bedrock change that addresses findings of the Sherlock contest related to
    /// deposit attributes. "Regolith" is the loose deposited rock that sits on top of Bedrock.
    /// Active if `regolith_time` != None && L2 block timestamp >= `Some(regolith_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub regolith_time: Option<u64>,
    /// `canyon_time` sets the activation time of the Canyon network upgrade.
    /// Active if `canyon_time` != None && L2 block timestamp >= `Some(canyon_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub canyon_time: Option<u64>,
    /// `delta_time` sets the activation time of the Delta network upgrade.
    /// Active if `delta_time` != None && L2 block timestamp >= `Some(delta_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub delta_time: Option<u64>,
    /// `ecotone_time` sets the activation time of the Ecotone network upgrade.
    /// Active if `ecotone_time` != None && L2 block timestamp >= `Some(ecotone_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub ecotone_time: Option<u64>,
    /// `fjord_time` sets the activation time of the Fjord network upgrade.
    /// Active if `fjord_time` != None && L2 block timestamp >= `Some(fjord_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub fjord_time: Option<u64>,
    /// `granite_time` sets the activation time for the Granite network upgrade.
    /// Active if `granite_time` != None && L2 block timestamp >= `Some(granite_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub granite_time: Option<u64>,
    /// `holocene_time` sets the activation time for the Holocene network upgrade.
    /// Active if `holocene_time` != None && L2 block timestamp >= `Some(holocene_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub holocene_time: Option<u64>,
    /// `pectra_blob_schedule_time` sets the activation time for the activation of the Pectra blob
    /// fee schedule for the L1 block info transaction. This is an optional fork, only present
    /// on Base sepolia chains that observed the L1 Pectra network upgrade with the reference node
    /// <=v1.11.1 sequencing the network.
    ///
    /// Active if `pectra_blob_schedule_time` != None && L2 block timestamp >=
    /// `Some(pectra_blob_schedule_time)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub pectra_blob_schedule_time: Option<u64>,
    /// `isthmus_time` sets the activation time for the Isthmus network upgrade.
    /// Active if `isthmus_time` != None && L2 block timestamp >= `Some(isthmus_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub isthmus_time: Option<u64>,
    /// `jovian_time` sets the activation time for the Jovian network upgrade.
    /// Active if `jovian_time` != None && L2 block timestamp >= `Some(jovian_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub jovian_time: Option<u64>,
    /// `base` contains Base-specific upgrade activation times.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BaseUpgradeConfig::is_empty")
    )]
    pub base: BaseUpgradeConfig,
}

impl Display for UpgradeConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[inline(always)]
        fn fmt_time(t: Option<u64>) -> String {
            t.map(|t| t.to_string()).unwrap_or_else(|| "Not scheduled".to_string())
        }

        writeln!(f, "🍴 Scheduled Upgrades:")?;
        for (name, time) in self.iter() {
            writeln!(f, "-> {} Activation Time: {}", name, fmt_time(time))?;
        }
        Ok(())
    }
}

impl UpgradeConfig {
    /// Returns an iterator of upgrade names -> their activation times (if scheduled.)
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, Option<u64>)> {
        [
            ("Regolith", self.regolith_time),
            ("Canyon", self.canyon_time),
            ("Delta", self.delta_time),
            ("Ecotone", self.ecotone_time),
            ("Fjord", self.fjord_time),
            ("Granite", self.granite_time),
            ("Holocene", self.holocene_time),
            ("Pectra Blob Schedule", self.pectra_blob_schedule_time),
            ("Isthmus", self.isthmus_time),
            ("Jovian", self.jovian_time),
            ("Azul", self.base.azul),
            ("Beryl", self.base.beryl),
            ("Cobalt", self.base.cobalt),
        ]
        .into_iter()
    }
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;

    #[test]
    fn test_upgrades_deserialize_json() {
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

        let upgrades = UpgradeConfig {
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
            base: BaseUpgradeConfig::default(),
        };

        let deserialized: UpgradeConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(upgrades, deserialized);
    }

    #[test]
    fn test_upgrades_deserialize_new_field_fail_json() {
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

        let err = serde_json::from_str::<UpgradeConfig>(raw).unwrap_err();
        assert_eq!(err.classify(), serde_json::error::Category::Data);
    }

    #[test]
    fn test_upgrades_deserialize_toml() {
        let raw: &str = r#"
        canyon_time =  1699981200 # Tue 14 Nov 2023 17:00:00 UTC
        delta_time =   1703203200 # Fri 22 Dec 2023 00:00:00 UTC
        ecotone_time = 1708534800 # Wed 21 Feb 2024 17:00:00 UTC
        fjord_time =   1716998400 # Wed 29 May 2024 16:00:00 UTC
        granite_time = 1723478400 # Mon Aug 12 16:00:00 UTC 2024
        holocene_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        "#;

        let upgrades = UpgradeConfig {
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
            base: BaseUpgradeConfig::default(),
        };

        let deserialized: UpgradeConfig = toml::from_str(raw).unwrap();
        assert_eq!(upgrades, deserialized);
    }

    #[test]
    fn test_upgrades_deserialize_new_field_fail_toml() {
        let raw: &str = r#"
        canyon_time =  1699981200 # Tue 14 Nov 2023 17:00:00 UTC
        delta_time =   1703203200 # Fri 22 Dec 2023 00:00:00 UTC
        ecotone_time = 1708534800 # Wed 21 Feb 2024 17:00:00 UTC
        fjord_time =   1716998400 # Wed 29 May 2024 16:00:00 UTC
        granite_time = 1723478400 # Mon Aug 12 16:00:00 UTC 2024
        holocene_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        new_field_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        "#;
        toml::from_str::<UpgradeConfig>(raw).unwrap_err();
    }

    #[test]
    fn test_upgrades_iter() {
        let upgrades = UpgradeConfig {
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
            base: BaseUpgradeConfig { azul: Some(11), beryl: Some(12), cobalt: Some(13) },
        };

        let mut iter = upgrades.iter();
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
        assert_eq!(iter.next(), Some(("Azul", Some(11))));
        assert_eq!(iter.next(), Some(("Beryl", Some(12))));
        assert_eq!(iter.next(), Some(("Cobalt", Some(13))));
        assert_eq!(iter.next(), None);
    }
}
