use alloc::string::String;
use core::{
    borrow::Borrow,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use base_alloy_hardforks::OpHardfork;

use crate::HardForkConfig;

/// Sentinel: `ActivationCache` has not yet been resolved.
const UNCACHED: u64 = u64::MAX;
/// Sentinel: `ActivationCache` is resolved and the feature has no scheduled activation.
const NO_ACTIVATION: u64 = u64::MAX - 1;

/// Lock-free cache for a lazily resolved activation timestamp.
///
/// Wraps an [`AtomicU64`] with two sentinel values so that [`Feature`] can derive
/// `Clone`, `PartialEq`, and `Eq` normally. The wrapper's [`Default`] initialises
/// to [`UNCACHED`], so `#[serde(skip)]` on the field works without a custom path.
struct ActivationCache(AtomicU64);

impl ActivationCache {
    fn load(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    fn store(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }
}

impl Default for ActivationCache {
    fn default() -> Self {
        Self(AtomicU64::new(UNCACHED))
    }
}

impl Clone for ActivationCache {
    fn clone(&self) -> Self {
        Self(AtomicU64::new(self.load()))
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
            return if cached == NO_ACTIVATION { None } else { Some(cached) };
        }
        let value = self.hardfork.and_then(|hf| hardforks.timestamp_for(hf));
        self.activation_cache.store(value.map_or(NO_ACTIVATION, |t| t));
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
