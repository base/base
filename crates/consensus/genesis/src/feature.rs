use alloc::{collections::BTreeMap, string::String};
use core::{
    borrow::Borrow,
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
};

use base_alloy_hardforks::OpHardfork;

use crate::HardForkConfig;

/// Sentinel: `ActivationCache` has not yet been resolved.
const UNCACHED: u32 = u32::MAX;
/// Sentinel: `ActivationCache` is resolved and the feature has no scheduled activation.
const NO_ACTIVATION: u32 = u32::MAX - 1;

/// Lock-free cache for a lazily resolved activation timestamp.
///
/// Wraps an [`AtomicU32`] with two sentinel values so that [`Feature`] can derive
/// `Clone`, `PartialEq`, and `Eq` normally. The wrapper's [`Default`] initialises
/// to [`UNCACHED`], so `#[serde(skip)]` on the field works without a custom path.
///
/// Timestamps are stored as `u32` for compatibility with 32-bit `no_std` targets
/// (e.g. `riscv32imac-unknown-none-elf`). All realistic hardfork timestamps fit
/// within `u32` (valid until year 2106).
struct ActivationCache(AtomicU32);

impl ActivationCache {
    fn load(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    fn store(&self, value: u32) {
        self.0.store(value, Ordering::Relaxed);
    }
}

impl Default for ActivationCache {
    fn default() -> Self {
        Self(AtomicU32::new(UNCACHED))
    }
}

impl Clone for ActivationCache {
    fn clone(&self) -> Self {
        Self(AtomicU32::new(self.load()))
    }
}

impl PartialEq for ActivationCache {
    /// Cache is derived state — two [`Feature`]s with the same config fields are equal
    /// regardless of whether and when the cache was populated.
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for ActivationCache {}

impl fmt::Debug for ActivationCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.load() {
            UNCACHED => f.write_str("Uncached"),
            NO_ACTIVATION => f.write_str("NoActivation"),
            t => write!(f, "Cached({t})"),
        }
    }
}

/// Opaque identifier for a named protocol feature.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Ident(pub String);

impl Ident {
    /// Creates a new [`Ident`] from a string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for Ident {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// A named protocol feature with an optional hardfork activation.
///
/// Features default to inactive (`hardfork: None`). Set `hardfork` to the
/// fork whose timestamp should activate this feature.
///
/// The activation timestamp is resolved lazily on the first call to
/// [`activation_time`](Feature::activation_time) and cached for all subsequent
/// calls — so the `HardForkConfig` lookup happens at most once per feature.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Feature {
    /// Machine-readable identifier (also the map key in
    /// [`RollupConfig::features`](crate::RollupConfig::features)).
    pub id: Ident,
    /// Human-readable description for logging and auditing.
    pub reason: String,
    /// The hardfork whose timestamp activates this feature. `None` = not scheduled.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub hardfork: Option<OpHardfork>,
    /// Lazily cached activation timestamp. Skipped by serde; initialised to
    /// [`UNCACHED`] via [`ActivationCache::default`].
    ///
    /// Multiple threads may race to populate this on first access, but the
    /// computation is deterministic so the stored value is always correct.
    #[cfg_attr(feature = "serde", serde(skip))]
    activation_cache: ActivationCache,
}

impl Feature {
    /// Creates a new [`Feature`] with the given id, reason, and optional hardfork activation.
    pub fn new(
        id: impl Into<String>,
        reason: impl Into<String>,
        hardfork: Option<OpHardfork>,
    ) -> Self {
        Self {
            id: Ident(id.into()),
            reason: reason.into(),
            hardfork,
            activation_cache: ActivationCache::default(),
        }
    }

    /// Jovian L1 block info feature identifier.
    pub const L1_BLOCK_INFO: &str = "JovianL1BlockInfo";
    /// DA footprint gas scalar feature identifier.
    pub const DA_FOOTPRINT_GAS_SCALAR: &str = "DaFootprintGasScalar";
    /// Minimum base fee feature identifier.
    pub const MIN_BASE_FEE: &str = "MinBaseFee";
    /// Operator fee multiplier feature identifier.
    pub const OPERATOR_FEE_MULTIPLIER: &str = "OperatorFeeMultiplier";
    /// DA footprint receipts feature identifier.
    pub const DA_FOOTPRINT_RECEIPTS: &str = "DaFootprintReceipts";
    /// DA footprint base fee feature identifier.
    pub const DA_FOOTPRINT_BASE_FEE: &str = "DaFootprintBaseFee";

    /// Returns the activation timestamp for this feature, lazily resolving it from
    /// `hardforks` on the first call and returning the cached value on all subsequent calls.
    ///
    /// Returns `None` if the feature has no hardfork assigned or the hardfork has no
    /// scheduled activation timestamp.
    ///
    /// # Cache invalidation
    ///
    /// The resolved timestamp is cached after the first call. If the [`HardForkConfig`]
    /// is mutated after this method has been called (e.g. in tests or hypothetical
    /// config-reload scenarios), the cached value will be stale. Call
    /// [`invalidate_cache`](Feature::invalidate_cache) before re-querying to force
    /// re-resolution from the updated config.
    pub fn activation_time(&self, hardforks: &HardForkConfig) -> Option<u64> {
        let cached = self.activation_cache.load();
        if cached != UNCACHED {
            return if cached == NO_ACTIVATION { None } else { Some(u64::from(cached)) };
        }
        let value = self.hardfork.and_then(|hf| hardforks.timestamp_for(hf));
        // Timestamps are stored as u32. All realistic hardfork timestamps fit within u32
        // (valid until year 2106). Values beyond that saturate to u32::MAX - 2, which
        // preserves the sentinel gap and still activates the feature (timestamp >= t).
        self.activation_cache
            .store(value.map_or(NO_ACTIVATION, |t| u32::try_from(t).unwrap_or(u32::MAX - 2)));
        value
    }

    /// Resets the activation timestamp cache, forcing the next call to
    /// [`activation_time`](Feature::activation_time) to re-resolve from the
    /// [`HardForkConfig`].
    ///
    /// Call this after mutating [`HardForkConfig`] fields that this feature's
    /// [`hardfork`](Feature::hardfork) depends on.
    pub fn invalidate_cache(&self) {
        self.activation_cache.store(UNCACHED);
    }
}

/// A map of named protocol features, pre-populated with the canonical Jovian feature set.
///
/// [`Default`]/[`FeatureMap::new`] inserts every known Jovian feature tied to
/// [`OpHardfork::Jovian`], so any [`RollupConfig`](crate::RollupConfig) built from
/// `Default` or deserialized without an explicit `features` key will activate those
/// features at the Jovian timestamp — matching the behaviour of the legacy
/// `is_jovian_active` calls they replaced.
///
/// Use [`FeatureMap::empty`] for `const` items or tests that need an unpopulated map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureMap(BTreeMap<Ident, Feature>);

impl FeatureMap {
    /// Creates a [`FeatureMap`] pre-populated with all canonical Jovian features.
    ///
    /// Equivalent to [`Default::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty [`FeatureMap`] with no features.
    ///
    /// This is a `const fn` and is suitable for use in `const` items (e.g. test-utility
    /// rollup configs). Prefer [`FeatureMap::new`] for runtime construction.
    pub const fn empty() -> Self {
        Self(BTreeMap::new())
    }
}

impl Default for FeatureMap {
    /// Returns a [`FeatureMap`] containing every canonical Jovian feature, each
    /// mapped to [`OpHardfork::Jovian`].
    fn default() -> Self {
        let map = [
            (Feature::L1_BLOCK_INFO, "Jovian L1 block info encoding"),
            (Feature::DA_FOOTPRINT_GAS_SCALAR, "DA footprint gas scalar in block header"),
            (Feature::MIN_BASE_FEE, "minimum base fee in extra data"),
            (Feature::OPERATOR_FEE_MULTIPLIER, "operator fee multiplier"),
            (Feature::DA_FOOTPRINT_RECEIPTS, "DA footprint in receipts"),
            (Feature::DA_FOOTPRINT_BASE_FEE, "DA footprint base fee"),
        ]
        .into_iter()
        .map(|(id, reason)| (Ident::new(id), Feature::new(id, reason, Some(OpHardfork::Jovian))))
        .collect();
        Self(map)
    }
}

impl Deref for FeatureMap {
    type Target = BTreeMap<Ident, Feature>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for FeatureMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for FeatureMap {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for FeatureMap {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Deserialize overrides then merge them onto the populated defaults.
        // This ensures both a missing `features` key and an explicit `"features": {}`
        // produce the same default-populated map — features intentionally omitted from
        // the config are not silently disabled.
        let overrides = BTreeMap::<Ident, Feature>::deserialize(d)?;
        let mut map = Self::default();
        map.0.extend(overrides);
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use base_alloy_hardforks::OpHardfork;

    use super::*;
    use crate::HardForkConfig;

    #[test]
    fn feature_map_default_contains_all_jovian_features() {
        let map = FeatureMap::default();
        for id in [
            Feature::L1_BLOCK_INFO,
            Feature::DA_FOOTPRINT_GAS_SCALAR,
            Feature::MIN_BASE_FEE,
            Feature::OPERATOR_FEE_MULTIPLIER,
            Feature::DA_FOOTPRINT_RECEIPTS,
            Feature::DA_FOOTPRINT_BASE_FEE,
        ] {
            let feature =
                map.get(id).unwrap_or_else(|| panic!("{id} missing from FeatureMap::default"));
            assert_eq!(
                feature.hardfork,
                Some(OpHardfork::Jovian),
                "{id} should be mapped to OpHardfork::Jovian"
            );
        }
    }

    #[test]
    fn feature_map_empty_has_no_entries() {
        assert!(FeatureMap::empty().is_empty());
    }

    #[test]
    fn feature_map_new_equals_default() {
        assert_eq!(FeatureMap::new(), FeatureMap::default());
    }

    #[test]
    fn feature_activation_time_scheduled() {
        let hf = HardForkConfig { jovian_time: Some(500), ..Default::default() };
        let feat = Feature::new("test", "test", Some(OpHardfork::Jovian));
        assert_eq!(feat.activation_time(&hf), Some(500));
        // Second call returns the same value from the cache.
        assert_eq!(feat.activation_time(&hf), Some(500));
    }

    #[test]
    fn feature_activation_time_no_hardfork() {
        let hf = HardForkConfig { jovian_time: Some(500), ..Default::default() };
        let feat = Feature::new("test", "test", None);
        assert_eq!(feat.activation_time(&hf), None);
    }

    #[test]
    fn feature_activation_time_hardfork_not_in_config() {
        // Hardfork variant assigned but jovian_time is not set in HardForkConfig.
        let hf = HardForkConfig::default();
        let feat = Feature::new("test", "test", Some(OpHardfork::Jovian));
        assert_eq!(feat.activation_time(&hf), None);
    }

    #[test]
    fn feature_invalidate_cache_forces_re_resolution() {
        let hf1 = HardForkConfig { jovian_time: Some(100), ..Default::default() };
        let feat = Feature::new("test", "test", Some(OpHardfork::Jovian));
        assert_eq!(feat.activation_time(&hf1), Some(100));

        feat.invalidate_cache();

        let hf2 = HardForkConfig { jovian_time: Some(200), ..Default::default() };
        assert_eq!(feat.activation_time(&hf2), Some(200));
    }

    #[cfg(feature = "serde")]
    mod serde_tests {
        use super::*;

        #[test]
        fn feature_map_serde_missing_key_yields_defaults() {
            #[derive(serde::Deserialize)]
            struct Wrap {
                #[serde(default)]
                features: FeatureMap,
            }
            let w: Wrap = serde_json::from_str("{}").unwrap();
            assert_eq!(w.features.len(), 6);
        }

        #[test]
        fn feature_map_serde_empty_object_yields_defaults() {
            // An explicit `"features": {}` must not wipe the defaults — it should merge
            // (empty override) on top of the pre-populated map.
            let fm: FeatureMap = serde_json::from_str("{}").unwrap();
            assert_eq!(fm.len(), 6, "empty JSON object must not wipe FeatureMap defaults");
        }

        #[test]
        fn feature_map_serde_partial_override_merges() {
            // One feature overridden with hardfork: null; the remaining 5 stay from defaults.
            let json = serde_json::json!({
                "MinBaseFee": {"id": "MinBaseFee", "reason": "overridden"}
            });
            let fm: FeatureMap = serde_json::from_value(json).unwrap();
            assert_eq!(fm.len(), 6, "partial override should not remove default features");
            let overridden = fm.get(Feature::MIN_BASE_FEE).unwrap();
            assert_eq!(overridden.hardfork, None, "overridden feature should have hardfork: None");
            let other = fm.get(Feature::L1_BLOCK_INFO).unwrap();
            assert_eq!(
                other.hardfork,
                Some(OpHardfork::Jovian),
                "non-overridden features should keep their Jovian mapping"
            );
        }

        #[test]
        fn feature_map_serde_round_trip() {
            let original = FeatureMap::new();
            let json = serde_json::to_string(&original).unwrap();
            let deserialized: FeatureMap = serde_json::from_str(&json).unwrap();
            assert_eq!(original, deserialized);
        }
    }
}
