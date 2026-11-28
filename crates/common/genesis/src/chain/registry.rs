//! The process-global registry of runtime upgrade activation overrides, which the L1
//! upgrade signal fills and [`RollupConfig`](crate::RollupConfig) and
//! [`BaseUpgrade::from_chain_and_timestamp`] consult. Behind the `runtime-registry` feature: a
//! build that cannot write the registry reads it as empty, so leaving it out changes nothing.

use alloc::collections::BTreeMap;

use spin::RwLock;

use crate::{BaseUpgrade, UpgradeActivation, UpgradeActivationOverrides};

/// Versioned runtime upgrade activation overrides for one chain.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct RuntimeUpgradeRegistryEntry {
    /// Runtime upgrade activation overrides.
    pub overrides: UpgradeActivationOverrides,
    /// Latest L1 block number whose schedule was applied to the overrides.
    pub last_updated_block_number: Option<u64>,
}

/// Process-global runtime upgrade activation overrides, keyed by chain ID.
///
/// Only reachable through the associated functions on [`RuntimeUpgradeRegistry`].
static REGISTRY: RwLock<BTreeMap<u64, RuntimeUpgradeRegistryEntry>> = RwLock::new(BTreeMap::new());

/// Process-local runtime upgrade activation registry.
///
/// The runtime upgrade signal treats the L1 contract as the authoritative source for these
/// overrides, so schedule application may replace the entire override set for a chain rather than
/// merging with previously stored entries.
///
/// The registry state lives in a process-global `spin::RwLock` that is only reachable through the
/// associated functions on [`RuntimeUpgradeRegistry`]; the lock is not exposed to callers.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeUpgradeRegistry;

impl RuntimeUpgradeRegistry {
    /// Returns the runtime activation override for a chain and contract upgrade ID.
    pub fn activation(chain_id: u64, upgrade_id: BaseUpgrade) -> Option<UpgradeActivation> {
        REGISTRY.read().get(&chain_id).and_then(|entry| entry.overrides.activation(upgrade_id))
    }

    /// Returns all runtime activation overrides for a chain.
    pub fn overrides(chain_id: u64) -> Option<UpgradeActivationOverrides> {
        REGISTRY.read().get(&chain_id).map(|entry| entry.overrides.clone())
    }

    /// Returns the latest L1 block number whose schedule was applied for a chain.
    pub fn last_updated_block_number(chain_id: u64) -> Option<u64> {
        REGISTRY.read().get(&chain_id).and_then(|entry| entry.last_updated_block_number)
    }

    /// Replaces all runtime activation overrides unless their L1 block predates stored state.
    ///
    /// The ordering check and replacement happen under one write lock so concurrent refreshes
    /// cannot overwrite a newer schedule with an older one. Returns `true` when the overrides were
    /// replaced and `false` when a stale schedule was rejected.
    pub fn replace_overrides(
        chain_id: u64,
        l1_block_number: u64,
        overrides: UpgradeActivationOverrides,
    ) -> bool {
        let mut registry = REGISTRY.write();
        if registry
            .get(&chain_id)
            .and_then(|entry| entry.last_updated_block_number)
            .is_some_and(|last_updated| l1_block_number < last_updated)
        {
            return false;
        }

        registry.insert(
            chain_id,
            RuntimeUpgradeRegistryEntry {
                overrides,
                last_updated_block_number: Some(l1_block_number),
            },
        );
        true
    }

    /// Clears all runtime activation overrides and their L1 block watermark for a chain.
    pub fn clear_chain(chain_id: u64) {
        REGISTRY.write().remove(&chain_id);
    }

    /// Removes one runtime activation override for a chain and contract upgrade ID.
    pub fn remove_activation_override(chain_id: u64, upgrade_id: BaseUpgrade) -> bool {
        let mut registry = REGISTRY.write();
        let Some(entry) = registry.get_mut(&chain_id) else {
            return false;
        };

        entry.overrides.remove_activation(upgrade_id)
    }

    /// Sets one runtime activation override for a chain and contract upgrade ID.
    pub fn set_activation(chain_id: u64, upgrade_id: BaseUpgrade, activation: UpgradeActivation) {
        let mut registry = REGISTRY.write();
        let entry = registry.entry(chain_id).or_default();
        entry.overrides.set_activation(upgrade_id, activation)
    }

    /// Sets one runtime timestamp activation override for a chain and contract upgrade ID.
    pub fn set_activation_timestamp(chain_id: u64, upgrade_id: BaseUpgrade, timestamp: u64) {
        Self::set_activation(chain_id, upgrade_id, UpgradeActivation::Timestamp(timestamp))
    }

    /// Activates Zenith for a chain via the runtime registry, bypassing the normal
    /// override block, for the lifetime of the returned test guard.
    #[cfg(any(test, feature = "test-utils"))]
    #[must_use = "the guard must be held for the duration of the test activation"]
    pub fn activate_zenith_for_testing(chain_id: u64, timestamp: u64) -> impl Drop {
        struct ZenithActivationGuard {
            chain_id: u64,
            previous: Option<UpgradeActivation>,
            remove_chain_if_empty: bool,
        }

        impl Drop for ZenithActivationGuard {
            fn drop(&mut self) {
                let mut registry = REGISTRY.write();
                let entry = registry.entry(self.chain_id).or_default();
                if let Some(previous) = self.previous {
                    entry.overrides.activations.insert(BaseUpgrade::Zenith, previous);
                } else {
                    entry.overrides.activations.remove(&BaseUpgrade::Zenith);
                }
                if self.remove_chain_if_empty && entry.overrides.is_empty() {
                    registry.remove(&self.chain_id);
                }
            }
        }

        let mut registry = REGISTRY.write();
        let remove_chain_if_empty = !registry.contains_key(&chain_id);
        let entry = registry.entry(chain_id).or_default();
        let previous = entry.overrides.activation(BaseUpgrade::Zenith);
        entry
            .overrides
            .activations
            .insert(BaseUpgrade::Zenith, UpgradeActivation::Timestamp(timestamp));
        ZenithActivationGuard { chain_id, previous, remove_chain_if_empty }
    }

    /// Sets one runtime override that clears a chain upgrade activation.
    pub fn clear_activation_timestamp(chain_id: u64, upgrade_id: BaseUpgrade) {
        Self::set_activation(chain_id, upgrade_id, UpgradeActivation::Never)
    }
}

#[cfg(test)]
mod tests {
    use spin::Mutex;

    use super::*;
    use crate::UpgradeConfig;

    static RUNTIME_REGISTRY_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn runtime_registry_tracks_timestamp_and_never_overrides() {
        let chain_id = 9_100_001;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Azul, 42);
        RuntimeUpgradeRegistry::clear_activation_timestamp(chain_id, BaseUpgrade::Beryl);
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Cobalt, 84);
        // Zenith is not contract-backed: the registry drops the write, so it is never stored.
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Zenith, u64::MAX);

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );
        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Zenith), None);
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Beryl),
            Some(UpgradeActivation::Never)
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Cobalt),
            Some(UpgradeActivation::Timestamp(84))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn known_chains_resolve_activation_boundaries() {
        let _guard = RUNTIME_REGISTRY_TEST_LOCK.lock();
        RuntimeUpgradeRegistry::clear_chain(8453);
        RuntimeUpgradeRegistry::clear_chain(84532);
        for (chain_id, config) in
            [(8453, UpgradeConfig::BASE_MAINNET), (84532, UpgradeConfig::BASE_SEPOLIA)]
        {
            let mut previous = BaseUpgrade::Bedrock;

            for upgrade in BaseUpgrade::EXECUTION_VARIANTS {
                let Some(activation) = config.activation_timestamp(upgrade) else {
                    continue;
                };

                if activation > 0 {
                    assert_eq!(
                        BaseUpgrade::from_chain_and_timestamp(chain_id, activation - 1),
                        Some(previous),
                        "chain {chain_id} immediately before {upgrade:?}",
                    );
                }

                assert_eq!(
                    BaseUpgrade::from_chain_and_timestamp(chain_id, activation),
                    Some(upgrade),
                    "chain {chain_id} at {upgrade:?} activation",
                );

                previous = upgrade;
            }
        }
    }

    #[test]
    fn activation_boundaries_respect_overrides() {
        let _guard = RUNTIME_REGISTRY_TEST_LOCK.lock();
        RuntimeUpgradeRegistry::clear_chain(8453);
        RuntimeUpgradeRegistry::clear_chain(84532);
        const CHAIN_ID: u64 = 8453;

        let beryl = UpgradeConfig::BASE_MAINNET.base.beryl.unwrap();
        let cobalt = beryl + 100;

        RuntimeUpgradeRegistry::set_activation_timestamp(CHAIN_ID, BaseUpgrade::Cobalt, cobalt);

        assert_eq!(
            BaseUpgrade::from_chain_and_timestamp(CHAIN_ID, cobalt - 1),
            Some(BaseUpgrade::Beryl)
        );
        assert_eq!(
            BaseUpgrade::from_chain_and_timestamp(CHAIN_ID, cobalt),
            Some(BaseUpgrade::Cobalt)
        );

        RuntimeUpgradeRegistry::clear_chain(CHAIN_ID);
        RuntimeUpgradeRegistry::clear_activation_timestamp(CHAIN_ID, BaseUpgrade::Beryl);

        assert_eq!(
            BaseUpgrade::from_chain_and_timestamp(CHAIN_ID, u64::MAX),
            Some(BaseUpgrade::Denim),
        );

        RuntimeUpgradeRegistry::clear_chain(CHAIN_ID);
    }

    #[test]
    fn far_future_resolves_latest_scheduled_upgrade() {
        let _guard = RUNTIME_REGISTRY_TEST_LOCK.lock();
        RuntimeUpgradeRegistry::clear_chain(8453);
        RuntimeUpgradeRegistry::clear_chain(84532);
        assert_eq!(BaseUpgrade::from_chain_and_timestamp(8453, u64::MAX), Some(BaseUpgrade::Denim));
        assert_eq!(
            BaseUpgrade::from_chain_and_timestamp(84532, u64::MAX),
            Some(BaseUpgrade::Denim)
        );
    }
}
