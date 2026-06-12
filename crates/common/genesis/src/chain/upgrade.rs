//! Contains the upgrade configuration for the chain.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
};
use core::fmt::Display;

use spin::{Once, RwLock, RwLockReadGuard, RwLockWriteGuard};

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

/// Runtime upgrade activation override.
#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub enum UpgradeActivation {
    /// The upgrade is not activated.
    Never,
    /// The upgrade activates at the given L2 timestamp.
    Timestamp(u64),
}

impl UpgradeActivation {
    /// Converts an optional timestamp into an upgrade activation.
    pub const fn from_timestamp(timestamp: Option<u64>) -> Self {
        match timestamp {
            Some(timestamp) => Self::Timestamp(timestamp),
            None => Self::Never,
        }
    }

    /// Returns the activation timestamp, if the upgrade is timestamp-activated.
    pub const fn timestamp(self) -> Option<u64> {
        match self {
            Self::Never => None,
            Self::Timestamp(timestamp) => Some(timestamp),
        }
    }
}

/// A target that can receive contract-backed upgrade activation updates.
///
/// Implemented by every schedule destination (rollup config, execution chain spec, runtime
/// registry) so a single applier can drive them all without per-target apply loops.
pub trait UpgradeActivationSink {
    /// Error returned when an activation cannot be applied to this target.
    type Error;

    /// Applies `activation` for the canonical contract upgrade ID.
    ///
    /// Returns `true` when the upgrade ID is supported by this target, `false` when it is
    /// unknown and was ignored.
    fn apply_activation(
        &mut self,
        upgrade_id: &str,
        activation: UpgradeActivation,
    ) -> Result<bool, Self::Error>;

    /// Finalizes the target after a batch of activations (e.g. recompute derived state).
    fn finalize(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Runtime upgrade activation overrides for one chain.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct UpgradeActivationOverrides {
    /// Upgrade activations keyed by canonical contract upgrade ID.
    pub activations: BTreeMap<String, UpgradeActivation>,
}

impl UpgradeActivationOverrides {
    /// Creates empty runtime upgrade activation overrides.
    pub const fn new() -> Self {
        Self { activations: BTreeMap::new() }
    }

    /// Returns true if no runtime overrides are configured.
    pub fn is_empty(&self) -> bool {
        self.activations.is_empty()
    }

    /// Returns the runtime activation override for a contract upgrade ID.
    pub fn activation(&self, upgrade_id: &str) -> Option<UpgradeActivation> {
        let hardfork_id = HardForkConfig::canonical_hardfork_id(upgrade_id)?;
        self.activations.get(hardfork_id).copied()
    }

    /// Removes the runtime activation override for a contract upgrade ID.
    pub fn remove_activation(&mut self, upgrade_id: &str) -> bool {
        let Some(hardfork_id) = HardForkConfig::canonical_hardfork_id(upgrade_id) else {
            return false;
        };

        self.activations.remove(hardfork_id).is_some()
    }

    /// Sets the runtime activation override for a contract upgrade ID.
    pub fn set_activation(&mut self, upgrade_id: &str, activation: UpgradeActivation) -> bool {
        let Some(hardfork_id) = HardForkConfig::canonical_hardfork_id(upgrade_id) else {
            return false;
        };

        self.activations.insert(hardfork_id.to_string(), activation);
        true
    }

    /// Sets a runtime timestamp activation override for a contract upgrade ID.
    pub fn set_activation_timestamp(&mut self, upgrade_id: &str, timestamp: u64) -> bool {
        self.set_activation(upgrade_id, UpgradeActivation::Timestamp(timestamp))
    }

    /// Sets a runtime override that clears an upgrade activation.
    pub fn clear_activation_timestamp(&mut self, upgrade_id: &str) -> bool {
        self.set_activation(upgrade_id, UpgradeActivation::Never)
    }
}

/// Process-local runtime upgrade activation registry.
///
/// The runtime upgrade signal treats the L1 contract as the authoritative source for these
/// overrides, so schedule application may replace the entire override set for a chain rather than
/// merging with previously stored entries.
///
/// This registry uses `spin::RwLock`, which cannot be locked again by the same thread. Do not
/// keep a read or write guard alive while calling another [`RuntimeUpgradeRegistry`] method, or
/// the thread will wait on its own lock forever.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeUpgradeRegistry;

impl RuntimeUpgradeRegistry {
    /// Returns the global runtime upgrade activation registry.
    pub fn registry() -> &'static RwLock<BTreeMap<u64, UpgradeActivationOverrides>> {
        static REGISTRY: Once<RwLock<BTreeMap<u64, UpgradeActivationOverrides>>> = Once::new();
        REGISTRY.call_once(|| RwLock::new(BTreeMap::new()))
    }

    /// Returns a registry read guard.
    fn read_registry() -> RwLockReadGuard<'static, BTreeMap<u64, UpgradeActivationOverrides>> {
        Self::registry().read()
    }

    /// Returns a registry write guard.
    fn write_registry() -> RwLockWriteGuard<'static, BTreeMap<u64, UpgradeActivationOverrides>> {
        Self::registry().write()
    }

    /// Returns the runtime activation override for a chain and contract upgrade ID.
    pub fn activation(chain_id: u64, upgrade_id: &str) -> Option<UpgradeActivation> {
        Self::read_registry().get(&chain_id).and_then(|overrides| overrides.activation(upgrade_id))
    }

    /// Returns all runtime activation overrides for a chain.
    pub fn overrides(chain_id: u64) -> Option<UpgradeActivationOverrides> {
        Self::read_registry().get(&chain_id).cloned()
    }

    /// Updates one chain's runtime activation overrides while holding the registry write lock.
    pub fn update_chain<R>(
        chain_id: u64,
        f: impl FnOnce(&mut UpgradeActivationOverrides) -> R,
    ) -> R {
        let mut registry = Self::write_registry();
        let result = {
            let overrides = registry.entry(chain_id).or_default();
            f(overrides)
        };

        if registry.get(&chain_id).is_some_and(UpgradeActivationOverrides::is_empty) {
            registry.remove(&chain_id);
        }

        result
    }

    /// Replaces all runtime activation overrides for a chain.
    pub fn replace_overrides(chain_id: u64, overrides: UpgradeActivationOverrides) {
        Self::write_registry().insert(chain_id, overrides);
    }

    /// Clears all runtime activation overrides for a chain.
    pub fn clear_chain(chain_id: u64) {
        Self::write_registry().remove(&chain_id);
    }

    /// Removes one runtime activation override for a chain and contract upgrade ID.
    pub fn remove_activation_override(chain_id: u64, upgrade_id: &str) -> bool {
        let mut registry = Self::write_registry();
        let Some(overrides) = registry.get_mut(&chain_id) else {
            return false;
        };

        overrides.remove_activation(upgrade_id)
    }

    /// Sets one runtime activation override for a chain and contract upgrade ID.
    pub fn set_activation(chain_id: u64, upgrade_id: &str, activation: UpgradeActivation) -> bool {
        let mut registry = Self::write_registry();
        let overrides = registry.entry(chain_id).or_default();
        overrides.set_activation(upgrade_id, activation)
    }

    /// Sets one runtime timestamp activation override for a chain and contract upgrade ID.
    pub fn set_activation_timestamp(chain_id: u64, upgrade_id: &str, timestamp: u64) -> bool {
        Self::set_activation(chain_id, upgrade_id, UpgradeActivation::Timestamp(timestamp))
    }

    /// Sets one runtime override that clears a chain upgrade activation.
    pub fn clear_activation_timestamp(chain_id: u64, upgrade_id: &str) -> bool {
        Self::set_activation(chain_id, upgrade_id, UpgradeActivation::Never)
    }
}

/// Hardfork configuration.
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

impl HardForkConfig {
    /// Canonical contract IDs supported by the runtime upgrade signal.
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
        "cobalt",
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

    /// Clears a timestamp-based activation time by contract hardfork ID.
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
            Some("cobalt") => self.base.cobalt = None,
            _ => return false,
        }

        true
    }

    /// Applies an upgrade activation override by contract upgrade ID.
    pub fn set_activation(&mut self, upgrade_id: &str, activation: UpgradeActivation) -> bool {
        match activation {
            UpgradeActivation::Never => self.clear_activation_timestamp(upgrade_id),
            UpgradeActivation::Timestamp(timestamp) => {
                self.set_activation_timestamp(upgrade_id, timestamp)
            }
        }
    }

    /// Applies all upgrade activation overrides.
    pub fn apply_activation_overrides(&mut self, overrides: &UpgradeActivationOverrides) {
        for (upgrade_id, activation) in &overrides.activations {
            self.set_activation(upgrade_id, *activation);
        }
    }

    /// Returns the activation for a timestamp-based contract upgrade ID.
    pub fn activation(&self, upgrade_id: &str) -> Option<UpgradeActivation> {
        let timestamp = match Self::canonical_hardfork_id(upgrade_id) {
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
            Some("cobalt") => self.base.cobalt,
            _ => return None,
        };

        Some(UpgradeActivation::from_timestamp(timestamp))
    }

    /// Returns the activation timestamp for a timestamp-based contract upgrade ID.
    pub fn activation_timestamp(&self, upgrade_id: &str) -> Option<u64> {
        self.activation(upgrade_id).and_then(UpgradeActivation::timestamp)
    }

    /// Sets a timestamp-based activation time by contract upgrade ID.
    pub fn set_activation_timestamp(&mut self, upgrade_id: &str, timestamp: u64) -> bool {
        match Self::canonical_hardfork_id(upgrade_id) {
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
            Some("cobalt") => self.base.cobalt = Some(timestamp),
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
            "cobalt" | "basecobalt" | "v3" => Some("cobalt"),
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

    #[test]
    fn test_set_activation_timestamp_by_hardfork_id() {
        let mut hardforks = HardForkConfig::default();

        assert!(hardforks.set_activation_timestamp("regolith", 1));
        assert!(hardforks.set_activation_timestamp("pectra-blob-schedule", 2));
        assert!(hardforks.set_activation_timestamp("base_azul", 3));
        assert!(hardforks.set_activation_timestamp("v2", 4));
        assert!(hardforks.set_activation_timestamp("v3", 5));
        assert!(!hardforks.set_activation_timestamp("unknown", 6));

        assert_eq!(hardforks.regolith_time, Some(1));
        assert_eq!(hardforks.pectra_blob_schedule_time, Some(2));
        assert_eq!(hardforks.base.azul, Some(3));
        assert_eq!(hardforks.base.beryl, Some(4));
        assert_eq!(hardforks.base.cobalt, Some(5));

        assert!(hardforks.clear_activation_timestamp("base_azul"));
        assert_eq!(hardforks.base.azul, None);
        assert_eq!(hardforks.base.beryl, Some(4));
        assert_eq!(hardforks.base.cobalt, Some(5));
        assert!(!hardforks.clear_activation_timestamp("unknown"));

        hardforks.clear_activation_timestamps();

        assert_eq!(hardforks, HardForkConfig::default());
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[test]
    fn runtime_registry_tracks_timestamp_and_never_overrides() {
        let chain_id = 9_100_001;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        assert!(RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, "base_azul", 42));
        assert!(RuntimeUpgradeRegistry::clear_activation_timestamp(chain_id, "beryl"));
        assert!(RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, "v3", 84));
        assert!(!RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, "unknown", 10));

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, "azul"),
            Some(UpgradeActivation::Timestamp(42))
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, "beryl"),
            Some(UpgradeActivation::Never)
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, "cobalt"),
            Some(UpgradeActivation::Timestamp(84))
        );
        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, "unknown"), None);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn hardfork_config_applies_activation_overrides() {
        let mut hardforks = HardForkConfig { canyon_time: Some(10), ..Default::default() };
        let mut overrides = UpgradeActivationOverrides::new();

        assert!(overrides.clear_activation_timestamp("canyon"));
        assert!(overrides.set_activation_timestamp("azul", 42));
        assert!(overrides.set_activation_timestamp("cobalt", 84));

        hardforks.apply_activation_overrides(&overrides);

        assert_eq!(hardforks.canyon_time, None);
        assert_eq!(hardforks.base.azul, Some(42));
        assert_eq!(hardforks.base.cobalt, Some(84));
    }
}
