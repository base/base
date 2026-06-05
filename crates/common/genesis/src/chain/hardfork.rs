//! Contains the hardfork configuration for the chain.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
};
use core::fmt::Display;
#[cfg(feature = "std")]
use std::sync::{OnceLock, RwLock};

/// Hardfork configuration for Base-specific upgrades.
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct HardforkConfig {
    /// `azul` sets the activation time for the Base Azul network upgrade.
    /// Active if `azul` != None && L2 block timestamp >= `Some(azul)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(alias = "v1", skip_serializing_if = "Option::is_none"))]
    pub azul: Option<u64>,
    /// `beryl` sets the activation time for the Beryl network upgrade.
    /// Active if `beryl` != None && L2 block timestamp >= `Some(beryl)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(alias = "v2", skip_serializing_if = "Option::is_none"))]
    pub beryl: Option<u64>,
}

impl HardforkConfig {
    /// Returns true if no Base-specific hardforks are configured.
    pub const fn is_empty(&self) -> bool {
        self.azul.is_none() && self.beryl.is_none()
    }
}

/// Runtime hardfork activation override.
#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub enum HardForkActivation {
    /// The hardfork is not activated.
    Never,
    /// The hardfork activates at the given L2 timestamp.
    Timestamp(u64),
}

impl HardForkActivation {
    /// Converts an optional timestamp into a hardfork activation.
    pub const fn from_timestamp(timestamp: Option<u64>) -> Self {
        match timestamp {
            Some(timestamp) => Self::Timestamp(timestamp),
            None => Self::Never,
        }
    }

    /// Returns the activation timestamp, if the hardfork is timestamp-activated.
    pub const fn timestamp(self) -> Option<u64> {
        match self {
            Self::Never => None,
            Self::Timestamp(timestamp) => Some(timestamp),
        }
    }
}

/// Runtime hardfork activation overrides for one chain.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct HardForkActivationOverrides {
    /// Hardfork activations keyed by canonical contract hardfork ID.
    pub activations: BTreeMap<String, HardForkActivation>,
}

impl HardForkActivationOverrides {
    /// Creates empty runtime hardfork activation overrides.
    pub fn new() -> Self {
        Self { activations: BTreeMap::new() }
    }

    /// Returns true if no runtime overrides are configured.
    pub fn is_empty(&self) -> bool {
        self.activations.is_empty()
    }

    /// Returns the runtime activation override for a hardfork ID.
    pub fn activation(&self, hardfork_id: &str) -> Option<HardForkActivation> {
        let hardfork_id = HardForkConfig::canonical_hardfork_id(hardfork_id)?;
        self.activations.get(hardfork_id).copied()
    }

    /// Removes the runtime activation override for a hardfork ID.
    pub fn remove_activation(&mut self, hardfork_id: &str) -> bool {
        let Some(hardfork_id) = HardForkConfig::canonical_hardfork_id(hardfork_id) else {
            return false;
        };

        self.activations.remove(hardfork_id).is_some()
    }

    /// Sets the runtime activation override for a hardfork ID.
    pub fn set_activation(&mut self, hardfork_id: &str, activation: HardForkActivation) -> bool {
        let Some(hardfork_id) = HardForkConfig::canonical_hardfork_id(hardfork_id) else {
            return false;
        };

        self.activations.insert(hardfork_id.to_string(), activation);
        true
    }

    /// Sets a runtime timestamp activation override for a hardfork ID.
    pub fn set_activation_timestamp(&mut self, hardfork_id: &str, timestamp: u64) -> bool {
        self.set_activation(hardfork_id, HardForkActivation::Timestamp(timestamp))
    }

    /// Sets a runtime override that clears a hardfork activation.
    pub fn clear_activation_timestamp(&mut self, hardfork_id: &str) -> bool {
        self.set_activation(hardfork_id, HardForkActivation::Never)
    }
}

/// Process-local runtime hardfork activation registry.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy)]
pub struct RuntimeHardForkRegistry;

#[cfg(feature = "std")]
impl RuntimeHardForkRegistry {
    /// Returns the global runtime hardfork activation registry.
    pub fn registry() -> &'static RwLock<BTreeMap<u64, HardForkActivationOverrides>> {
        static REGISTRY: OnceLock<RwLock<BTreeMap<u64, HardForkActivationOverrides>>> =
            OnceLock::new();
        REGISTRY.get_or_init(|| RwLock::new(BTreeMap::new()))
    }

    /// Returns the runtime activation override for a chain and hardfork ID.
    pub fn activation(chain_id: u64, hardfork_id: &str) -> Option<HardForkActivation> {
        Self::registry()
            .read()
            .expect("runtime hardfork registry poisoned")
            .get(&chain_id)
            .and_then(|overrides| overrides.activation(hardfork_id))
    }

    /// Returns all runtime activation overrides for a chain.
    pub fn overrides(chain_id: u64) -> Option<HardForkActivationOverrides> {
        Self::registry().read().expect("runtime hardfork registry poisoned").get(&chain_id).cloned()
    }

    /// Replaces all runtime activation overrides for a chain.
    pub fn replace_overrides(chain_id: u64, overrides: HardForkActivationOverrides) {
        Self::registry()
            .write()
            .expect("runtime hardfork registry poisoned")
            .insert(chain_id, overrides);
    }

    /// Clears all runtime activation overrides for a chain.
    pub fn clear_chain(chain_id: u64) {
        Self::registry().write().expect("runtime hardfork registry poisoned").remove(&chain_id);
    }

    /// Removes one runtime activation override for a chain and hardfork ID.
    pub fn remove_activation_override(chain_id: u64, hardfork_id: &str) -> bool {
        let mut registry = Self::registry().write().expect("runtime hardfork registry poisoned");
        let Some(overrides) = registry.get_mut(&chain_id) else {
            return false;
        };

        overrides.remove_activation(hardfork_id)
    }

    /// Sets one runtime activation override for a chain and hardfork ID.
    pub fn set_activation(
        chain_id: u64,
        hardfork_id: &str,
        activation: HardForkActivation,
    ) -> bool {
        let mut registry = Self::registry().write().expect("runtime hardfork registry poisoned");
        let overrides = registry.entry(chain_id).or_default();
        overrides.set_activation(hardfork_id, activation)
    }

    /// Sets one runtime timestamp activation override for a chain and hardfork ID.
    pub fn set_activation_timestamp(chain_id: u64, hardfork_id: &str, timestamp: u64) -> bool {
        Self::set_activation(chain_id, hardfork_id, HardForkActivation::Timestamp(timestamp))
    }

    /// Sets one runtime override that clears a chain hardfork activation.
    pub fn clear_activation_timestamp(chain_id: u64, hardfork_id: &str) -> bool {
        Self::set_activation(chain_id, hardfork_id, HardForkActivation::Never)
    }
}

/// Hardfork configuration.
///
/// See: <https://github.com/ethereum-optimism/superchain-registry/blob/8ff62ada16e14dd59d0fb94ffb47761c7fa96e01/ops/internal/config/chain.go#L102-L110>
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct HardForkConfig {
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
    /// `base` contains Base-specific hardfork activation times.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "HardforkConfig::is_empty")
    )]
    pub base: HardforkConfig,
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
    /// Canonical contract hardfork IDs supported by the runtime upgrade signal.
    pub const CONTRACT_HARDFORK_IDS: &'static [&'static str] = &[
        "regolith",
        "canyon",
        "delta",
        "ecotone",
        "fjord",
        "granite",
        "holocene",
        "pectra_blob_schedule",
        "isthmus",
        "jovian",
        "azul",
        "beryl",
    ];

    /// Clears all timestamp-based hardfork activation times.
    pub fn clear_activation_timestamps(&mut self) {
        self.regolith_time = None;
        self.canyon_time = None;
        self.delta_time = None;
        self.ecotone_time = None;
        self.fjord_time = None;
        self.granite_time = None;
        self.holocene_time = None;
        self.pectra_blob_schedule_time = None;
        self.isthmus_time = None;
        self.jovian_time = None;
        self.base = HardforkConfig::default();
    }

    /// Clears a timestamp-based hardfork activation time by contract hardfork ID.
    pub fn clear_activation_timestamp(&mut self, hardfork_id: &str) -> bool {
        match Self::canonical_hardfork_id(hardfork_id) {
            Some("regolith") => self.regolith_time = None,
            Some("canyon") => self.canyon_time = None,
            Some("delta") => self.delta_time = None,
            Some("ecotone") => self.ecotone_time = None,
            Some("fjord") => self.fjord_time = None,
            Some("granite") => self.granite_time = None,
            Some("holocene") => self.holocene_time = None,
            Some("pectra_blob_schedule") => self.pectra_blob_schedule_time = None,
            Some("isthmus") => self.isthmus_time = None,
            Some("jovian") => self.jovian_time = None,
            Some("azul") => self.base.azul = None,
            Some("beryl") => self.base.beryl = None,
            _ => return false,
        }

        true
    }

    /// Applies a hardfork activation override by contract hardfork ID.
    pub fn set_activation(&mut self, hardfork_id: &str, activation: HardForkActivation) -> bool {
        match activation {
            HardForkActivation::Never => self.clear_activation_timestamp(hardfork_id),
            HardForkActivation::Timestamp(timestamp) => {
                self.set_activation_timestamp(hardfork_id, timestamp)
            }
        }
    }

    /// Applies all hardfork activation overrides.
    pub fn apply_activation_overrides(&mut self, overrides: &HardForkActivationOverrides) {
        for (hardfork_id, activation) in &overrides.activations {
            self.set_activation(hardfork_id, *activation);
        }
    }

    /// Returns the activation for a timestamp-based hardfork ID.
    pub fn activation(&self, hardfork_id: &str) -> Option<HardForkActivation> {
        let timestamp = match Self::canonical_hardfork_id(hardfork_id) {
            Some("regolith") => self.regolith_time,
            Some("canyon") => self.canyon_time,
            Some("delta") => self.delta_time,
            Some("ecotone") => self.ecotone_time,
            Some("fjord") => self.fjord_time,
            Some("granite") => self.granite_time,
            Some("holocene") => self.holocene_time,
            Some("pectra_blob_schedule") => self.pectra_blob_schedule_time,
            Some("isthmus") => self.isthmus_time,
            Some("jovian") => self.jovian_time,
            Some("azul") => self.base.azul,
            Some("beryl") => self.base.beryl,
            _ => return None,
        };

        Some(HardForkActivation::from_timestamp(timestamp))
    }

    /// Returns the activation timestamp for a timestamp-based hardfork ID.
    pub fn activation_timestamp(&self, hardfork_id: &str) -> Option<u64> {
        self.activation(hardfork_id).and_then(HardForkActivation::timestamp)
    }

    /// Sets a timestamp-based hardfork activation time by contract hardfork ID.
    pub fn set_activation_timestamp(&mut self, hardfork_id: &str, timestamp: u64) -> bool {
        match Self::canonical_hardfork_id(hardfork_id) {
            Some("regolith") => self.regolith_time = Some(timestamp),
            Some("canyon") => self.canyon_time = Some(timestamp),
            Some("delta") => self.delta_time = Some(timestamp),
            Some("ecotone") => self.ecotone_time = Some(timestamp),
            Some("fjord") => self.fjord_time = Some(timestamp),
            Some("granite") => self.granite_time = Some(timestamp),
            Some("holocene") => self.holocene_time = Some(timestamp),
            Some("pectra_blob_schedule") => self.pectra_blob_schedule_time = Some(timestamp),
            Some("isthmus") => self.isthmus_time = Some(timestamp),
            Some("jovian") => self.jovian_time = Some(timestamp),
            Some("azul") => self.base.azul = Some(timestamp),
            Some("beryl") => self.base.beryl = Some(timestamp),
            _ => return false,
        }

        true
    }

    /// Returns the canonical contract hardfork ID for an input hardfork ID or alias.
    pub fn canonical_hardfork_id(hardfork_id: &str) -> Option<&'static str> {
        match Self::normalized_hardfork_id(hardfork_id).as_str() {
            "regolith" => Some("regolith"),
            "canyon" => Some("canyon"),
            "delta" => Some("delta"),
            "ecotone" => Some("ecotone"),
            "fjord" => Some("fjord"),
            "granite" => Some("granite"),
            "holocene" => Some("holocene"),
            "pectrablobschedule" => Some("pectra_blob_schedule"),
            "isthmus" => Some("isthmus"),
            "jovian" => Some("jovian"),
            "azul" | "baseazul" | "v1" => Some("azul"),
            "beryl" | "baseberyl" | "v2" => Some("beryl"),
            _ => None,
        }
    }

    /// Normalizes a contract hardfork ID for matching.
    pub fn normalized_hardfork_id(hardfork_id: &str) -> String {
        hardfork_id
            .bytes()
            .filter(|b| !matches!(b, b'_' | b'-' | b' '))
            .map(|b| b.to_ascii_lowercase() as char)
            .collect()
    }

    /// Returns an iterator of hardfork names -> their activation times (if scheduled.)
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
        ]
        .into_iter()
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

        let hardforks = HardForkConfig {
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
            base: HardforkConfig::default(),
        };

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

        let hardforks = HardForkConfig {
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
            base: HardforkConfig::default(),
        };

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
            base: HardforkConfig { azul: Some(11), beryl: Some(12) },
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
        assert_eq!(iter.next(), Some(("Azul", Some(11))));
        assert_eq!(iter.next(), Some(("Beryl", Some(12))));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_set_activation_timestamp_by_hardfork_id() {
        let mut hardforks = HardForkConfig::default();

        assert!(hardforks.set_activation_timestamp("regolith", 1));
        assert!(hardforks.set_activation_timestamp("pectra-blob-schedule", 2));
        assert!(hardforks.set_activation_timestamp("base_azul", 3));
        assert!(hardforks.set_activation_timestamp("v2", 4));
        assert!(!hardforks.set_activation_timestamp("unknown", 5));

        assert_eq!(hardforks.regolith_time, Some(1));
        assert_eq!(hardforks.pectra_blob_schedule_time, Some(2));
        assert_eq!(hardforks.base.azul, Some(3));
        assert_eq!(hardforks.base.beryl, Some(4));

        assert!(hardforks.clear_activation_timestamp("base_azul"));
        assert_eq!(hardforks.base.azul, None);
        assert_eq!(hardforks.base.beryl, Some(4));
        assert!(!hardforks.clear_activation_timestamp("unknown"));

        hardforks.clear_activation_timestamps();

        assert_eq!(hardforks, HardForkConfig::default());
    }
}

#[cfg(test)]
#[cfg(feature = "std")]
mod runtime_tests {
    use super::*;

    #[test]
    fn runtime_registry_tracks_timestamp_and_never_overrides() {
        let chain_id = 9_100_001;
        RuntimeHardForkRegistry::clear_chain(chain_id);

        assert!(RuntimeHardForkRegistry::set_activation_timestamp(chain_id, "base_azul", 42));
        assert!(RuntimeHardForkRegistry::clear_activation_timestamp(chain_id, "beryl"));
        assert!(!RuntimeHardForkRegistry::set_activation_timestamp(chain_id, "unknown", 10));

        assert_eq!(
            RuntimeHardForkRegistry::activation(chain_id, "azul"),
            Some(HardForkActivation::Timestamp(42))
        );
        assert_eq!(
            RuntimeHardForkRegistry::activation(chain_id, "beryl"),
            Some(HardForkActivation::Never)
        );
        assert_eq!(RuntimeHardForkRegistry::activation(chain_id, "unknown"), None);

        RuntimeHardForkRegistry::clear_chain(chain_id);
    }

    #[test]
    fn hardfork_config_applies_activation_overrides() {
        let mut hardforks = HardForkConfig { canyon_time: Some(10), ..Default::default() };
        let mut overrides = HardForkActivationOverrides::new();

        assert!(overrides.clear_activation_timestamp("canyon"));
        assert!(overrides.set_activation_timestamp("azul", 42));

        hardforks.apply_activation_overrides(&overrides);

        assert_eq!(hardforks.canyon_time, None);
        assert_eq!(hardforks.base.azul, Some(42));
    }
}
