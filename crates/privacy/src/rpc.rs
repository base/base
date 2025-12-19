//! Privacy-Aware RPC Filtering
//!
//! This module provides filtering logic for RPC responses to hide private
//! storage slots and events from unauthorized callers.
//!
//! # Design
//!
//! The filtering is designed to be:
//! - **Backwards compatible**: When no privacy registry is provided, all
//!   behavior is unchanged
//! - **Additive only**: New filtering is opt-in, not opt-out
//! - **Non-breaking**: Existing RPC calls work exactly as before
//!
//! # Usage
//!
//! ```ignore
//! use base_reth_privacy::rpc::{PrivacyRpcFilter, RpcFilterConfig};
//!
//! // Create filter with privacy awareness
//! let filter = PrivacyRpcFilter::new(Some(registry), Some(store));
//!
//! // Or without privacy (backwards compatible)
//! let filter = PrivacyRpcFilter::disabled();
//!
//! // Filter storage responses
//! let filtered_value = filter.filter_storage(contract, slot, value, caller);
//!
//! // Filter log responses
//! let filtered_logs = filter.filter_logs(logs, caller);
//! ```

use crate::{
    classification::{classify_slot, SlotClassification},
    registry::PrivacyRegistry,
    store::{PrivateStateStore, READ},
};
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::Log;
use std::sync::Arc;

/// Configuration for RPC privacy filtering.
#[derive(Debug, Clone, Default)]
pub struct RpcFilterConfig {
    /// Whether to return zero for unauthorized private slot reads.
    /// If false, returns the actual value (useful for debugging).
    pub hide_private_values: bool,
    /// Whether to filter out logs from contracts with `hide_events = true`.
    pub filter_private_logs: bool,
}

impl RpcFilterConfig {
    /// Create a new config with privacy filtering enabled.
    pub fn enabled() -> Self {
        Self {
            hide_private_values: true,
            filter_private_logs: true,
        }
    }

    /// Create a new config with privacy filtering disabled (pass-through).
    pub fn disabled() -> Self {
        Self {
            hide_private_values: false,
            filter_private_logs: false,
        }
    }
}

/// Privacy-aware RPC filter for storage and log responses.
///
/// When privacy components are not provided, all filtering is bypassed
/// for complete backwards compatibility.
#[derive(Debug, Clone)]
pub struct PrivacyRpcFilter {
    /// Privacy registry (if privacy is enabled)
    registry: Option<Arc<PrivacyRegistry>>,
    /// Private state store (if privacy is enabled)
    store: Option<Arc<PrivateStateStore>>,
    /// Filter configuration
    config: RpcFilterConfig,
}

impl Default for PrivacyRpcFilter {
    fn default() -> Self {
        Self::disabled()
    }
}

impl PrivacyRpcFilter {
    /// Create a new privacy filter with the given components.
    ///
    /// If `registry` is `None`, all filtering is disabled (backwards compatible).
    pub fn new(
        registry: Option<Arc<PrivacyRegistry>>,
        store: Option<Arc<PrivateStateStore>>,
    ) -> Self {
        let config = if registry.is_some() {
            RpcFilterConfig::enabled()
        } else {
            RpcFilterConfig::disabled()
        };

        Self {
            registry,
            store,
            config,
        }
    }

    /// Create a new privacy filter with custom configuration.
    pub fn with_config(
        registry: Option<Arc<PrivacyRegistry>>,
        store: Option<Arc<PrivateStateStore>>,
        config: RpcFilterConfig,
    ) -> Self {
        Self {
            registry,
            store,
            config,
        }
    }

    /// Create a disabled filter (complete pass-through, backwards compatible).
    pub fn disabled() -> Self {
        Self {
            registry: None,
            store: None,
            config: RpcFilterConfig::disabled(),
        }
    }

    /// Check if privacy filtering is enabled.
    pub fn is_enabled(&self) -> bool {
        self.registry.is_some() && self.config.hide_private_values
    }

    /// Filter a storage value response.
    ///
    /// # Arguments
    ///
    /// * `contract` - The contract address
    /// * `slot` - The storage slot being accessed
    /// * `value` - The actual storage value
    /// * `caller` - The address making the RPC request (if known)
    ///
    /// # Returns
    ///
    /// - The original `value` if:
    ///   - Privacy is disabled
    ///   - The slot is public
    ///   - The caller is authorized to read this slot
    /// - `B256::ZERO` if the slot is private and caller is not authorized
    ///
    /// # Backwards Compatibility
    ///
    /// When `registry` is `None`, this always returns the original value.
    pub fn filter_storage(
        &self,
        contract: Address,
        slot: U256,
        value: B256,
        caller: Option<Address>,
    ) -> B256 {
        // Fast path: no privacy filtering
        if !self.config.hide_private_values {
            return value;
        }

        let registry = match &self.registry {
            Some(r) => r,
            None => return value, // No registry = pass through
        };

        // Classify the slot
        match classify_slot(registry, contract, slot) {
            SlotClassification::Public => value,
            SlotClassification::Private { owner } => {
                // Check if caller is authorized
                if self.is_caller_authorized(contract, slot, owner, caller) {
                    value
                } else {
                    B256::ZERO
                }
            }
        }
    }

    /// Filter a list of logs, removing logs from private contracts.
    ///
    /// # Arguments
    ///
    /// * `logs` - The logs to filter
    /// * `caller` - The address making the RPC request (if known)
    ///
    /// # Returns
    ///
    /// A filtered list of logs, with private logs removed.
    ///
    /// # Backwards Compatibility
    ///
    /// When `registry` is `None`, this returns the original logs unchanged.
    pub fn filter_logs(&self, logs: Vec<Log>, caller: Option<Address>) -> Vec<Log> {
        // Fast path: no privacy filtering
        if !self.config.filter_private_logs {
            return logs;
        }

        let registry = match &self.registry {
            Some(r) => r,
            None => return logs, // No registry = pass through
        };

        logs.into_iter()
            .filter(|log| self.should_include_log(registry, log, caller))
            .collect()
    }

    /// Check if a log should be included in the response.
    fn should_include_log(
        &self,
        registry: &PrivacyRegistry,
        log: &Log,
        caller: Option<Address>,
    ) -> bool {
        let contract = log.address();

        // Get contract config
        let config = match registry.get_config(&contract) {
            Some(c) => c,
            None => return true, // Not a private contract, include it
        };

        // If hide_events is false, include all logs from this contract
        if !config.hide_events {
            return true;
        }

        // If hide_events is true, only include if caller is authorized
        match caller {
            Some(addr) if addr == config.admin => true,
            Some(addr) if addr == contract => true,
            _ => false,
        }
    }

    /// Check if a caller is authorized to read a private slot.
    fn is_caller_authorized(
        &self,
        contract: Address,
        slot: U256,
        owner: Address,
        caller: Option<Address>,
    ) -> bool {
        let caller = match caller {
            Some(c) => c,
            None => return false, // No caller info = not authorized
        };

        // Owner is always authorized
        if caller == owner {
            return true;
        }

        // Contract itself is always authorized
        if caller == contract {
            return true;
        }

        // Check explicit authorization in store
        if let Some(store) = &self.store {
            return store.is_authorized(contract, slot, caller, READ);
        }

        false
    }
}

/// Result of filtering a storage request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageFilterResult {
    /// Value is public or caller is authorized
    Allowed(B256),
    /// Value is private and caller is not authorized
    Denied,
}

impl StorageFilterResult {
    /// Get the value to return in the RPC response.
    pub fn value(&self) -> B256 {
        match self {
            Self::Allowed(v) => *v,
            Self::Denied => B256::ZERO,
        }
    }

    /// Check if access was denied.
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Denied)
    }
}

/// Detailed result of filtering a storage request.
impl PrivacyRpcFilter {
    /// Filter storage with detailed result.
    ///
    /// This is useful for logging/metrics when you need to know if access
    /// was denied vs the value actually being zero.
    pub fn filter_storage_detailed(
        &self,
        contract: Address,
        slot: U256,
        value: B256,
        caller: Option<Address>,
    ) -> StorageFilterResult {
        // Fast path: no privacy filtering
        if !self.config.hide_private_values {
            return StorageFilterResult::Allowed(value);
        }

        let registry = match &self.registry {
            Some(r) => r,
            None => return StorageFilterResult::Allowed(value),
        };

        match classify_slot(registry, contract, slot) {
            SlotClassification::Public => StorageFilterResult::Allowed(value),
            SlotClassification::Private { owner } => {
                if self.is_caller_authorized(contract, slot, owner, caller) {
                    StorageFilterResult::Allowed(value)
                } else {
                    StorageFilterResult::Denied
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{OwnershipType, PrivateContractConfig, SlotConfig, SlotType};
    use crate::store::AuthEntry;

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn setup_registry_with_private_contract(
        contract: Address,
        admin: Address,
        hide_events: bool,
    ) -> (Arc<PrivacyRegistry>, Arc<PrivateStateStore>) {
        let registry = Arc::new(PrivacyRegistry::new());
        let store = Arc::new(PrivateStateStore::new());

        let config = PrivateContractConfig {
            address: contract,
            admin,
            slots: vec![SlotConfig {
                base_slot: U256::from(5),
                slot_type: SlotType::Simple,
                ownership: OwnershipType::Contract,
            }],
            registered_at: 1,
            hide_events,
        };

        registry.register(config).unwrap();
        (registry, store)
    }

    // ==================== BACKWARDS COMPATIBILITY TESTS ====================

    #[test]
    fn test_disabled_filter_passes_all_storage() {
        let filter = PrivacyRpcFilter::disabled();

        let contract = test_address(1);
        let slot = U256::from(5);
        let value = B256::from(U256::from(12345));
        let caller = Some(test_address(99));

        // Even if this would be a private slot, disabled filter passes through
        let result = filter.filter_storage(contract, slot, value, caller);
        assert_eq!(result, value);
    }

    #[test]
    fn test_disabled_filter_passes_all_logs() {
        let filter = PrivacyRpcFilter::disabled();

        let logs = vec![
            create_test_log(test_address(1), U256::from(100)),
            create_test_log(test_address(2), U256::from(200)),
        ];
        let caller = Some(test_address(99));

        let result = filter.filter_logs(logs.clone(), caller);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_no_registry_is_backwards_compatible() {
        // Even with a store but no registry, should pass through
        let store = Arc::new(PrivateStateStore::new());
        let filter = PrivacyRpcFilter::new(None, Some(store));

        let contract = test_address(1);
        let slot = U256::from(5);
        let value = B256::from(U256::from(12345));

        let result = filter.filter_storage(contract, slot, value, None);
        assert_eq!(result, value);
    }

    // ==================== STORAGE FILTERING TESTS ====================

    #[test]
    fn test_public_slot_returns_value() {
        let contract = test_address(1);
        let admin = test_address(2);
        let (registry, store) = setup_registry_with_private_contract(contract, admin, false);

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        // Slot 10 is NOT registered as private (only slot 5 is)
        let slot = U256::from(10);
        let value = B256::from(U256::from(12345));

        let result = filter.filter_storage(contract, slot, value, None);
        assert_eq!(result, value, "Public slot should return actual value");
    }

    #[test]
    fn test_private_slot_unauthorized_returns_zero() {
        let contract = test_address(1);
        let admin = test_address(2);
        let (registry, store) = setup_registry_with_private_contract(contract, admin, false);

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        // Slot 5 IS registered as private
        let slot = U256::from(5);
        let value = B256::from(U256::from(12345));
        let unauthorized_caller = Some(test_address(99));

        let result = filter.filter_storage(contract, slot, value, unauthorized_caller);
        assert_eq!(result, B256::ZERO, "Private slot should return zero for unauthorized caller");
    }

    #[test]
    fn test_private_slot_no_caller_returns_zero() {
        let contract = test_address(1);
        let admin = test_address(2);
        let (registry, store) = setup_registry_with_private_contract(contract, admin, false);

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        let slot = U256::from(5);
        let value = B256::from(U256::from(12345));

        let result = filter.filter_storage(contract, slot, value, None);
        assert_eq!(result, B256::ZERO, "Private slot should return zero when no caller");
    }

    #[test]
    fn test_private_slot_owner_authorized() {
        let contract = test_address(1);
        let admin = test_address(2);
        let (registry, store) = setup_registry_with_private_contract(contract, admin, false);

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        // For Simple slots with OwnershipType::Contract, the contract is the owner
        let slot = U256::from(5);
        let value = B256::from(U256::from(12345));

        // Contract calling its own slot should be authorized
        let result = filter.filter_storage(contract, slot, value, Some(contract));
        assert_eq!(result, value, "Owner should be able to read private slot");
    }

    #[test]
    fn test_private_slot_delegated_access() {
        let contract = test_address(1);
        let admin = test_address(2);
        let delegate = test_address(10);
        let (registry, store) = setup_registry_with_private_contract(contract, admin, false);

        // Set up the store to recognize this slot and authorize the delegate
        let slot = U256::from(5);
        store.set(contract, slot, U256::from(12345), contract);
        store.authorize(
            contract,
            slot,
            delegate,
            AuthEntry::new(READ, 0, 1),
        );

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));
        let value = B256::from(U256::from(12345));

        // Delegate should be authorized
        let result = filter.filter_storage(contract, slot, value, Some(delegate));
        assert_eq!(result, value, "Delegated caller should be able to read");
    }

    #[test]
    fn test_unregistered_contract_is_public() {
        let contract = test_address(1);
        let unregistered_contract = test_address(99);
        let admin = test_address(2);
        let (registry, store) = setup_registry_with_private_contract(contract, admin, false);

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        // Access storage from an unregistered contract
        let slot = U256::from(5);
        let value = B256::from(U256::from(12345));

        let result = filter.filter_storage(unregistered_contract, slot, value, None);
        assert_eq!(result, value, "Unregistered contract slots are public");
    }

    // ==================== LOG FILTERING TESTS ====================

    #[test]
    fn test_public_contract_logs_included() {
        let private_contract = test_address(1);
        let public_contract = test_address(2);
        let admin = test_address(99);

        let (registry, store) = setup_registry_with_private_contract(
            private_contract,
            admin,
            true, // hide_events = true
        );

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        let logs = vec![
            create_test_log(public_contract, U256::from(100)),
        ];

        let result = filter.filter_logs(logs, None);
        assert_eq!(result.len(), 1, "Logs from unregistered contracts should be included");
    }

    #[test]
    fn test_private_contract_logs_hidden_when_hide_events() {
        let contract = test_address(1);
        let admin = test_address(99);

        let (registry, store) = setup_registry_with_private_contract(
            contract,
            admin,
            true, // hide_events = true
        );

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        let logs = vec![
            create_test_log(contract, U256::from(100)),
        ];

        // Random caller should not see logs from contract with hide_events=true
        let result = filter.filter_logs(logs, Some(test_address(50)));
        assert!(result.is_empty(), "Private contract logs should be hidden");
    }

    #[test]
    fn test_private_contract_logs_visible_to_admin() {
        let contract = test_address(1);
        let admin = test_address(99);

        let (registry, store) = setup_registry_with_private_contract(
            contract,
            admin,
            true, // hide_events = true
        );

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        let logs = vec![
            create_test_log(contract, U256::from(100)),
        ];

        // Admin should see logs even with hide_events=true
        let result = filter.filter_logs(logs, Some(admin));
        assert_eq!(result.len(), 1, "Admin should see private contract logs");
    }

    #[test]
    fn test_private_contract_logs_visible_when_not_hiding() {
        let contract = test_address(1);
        let admin = test_address(99);

        let (registry, store) = setup_registry_with_private_contract(
            contract,
            admin,
            false, // hide_events = false
        );

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        let logs = vec![
            create_test_log(contract, U256::from(100)),
        ];

        // Anyone should see logs when hide_events=false
        let result = filter.filter_logs(logs, Some(test_address(50)));
        assert_eq!(result.len(), 1, "Logs should be visible when hide_events=false");
    }

    #[test]
    fn test_mixed_log_filtering() {
        let private_contract = test_address(1);
        let public_contract = test_address(2);
        let admin = test_address(99);

        let (registry, store) = setup_registry_with_private_contract(
            private_contract,
            admin,
            true, // hide_events = true for private_contract
        );

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        let logs = vec![
            create_test_log(public_contract, U256::from(1)),  // Should be included
            create_test_log(private_contract, U256::from(2)), // Should be filtered
            create_test_log(public_contract, U256::from(3)),  // Should be included
        ];

        // Random caller
        let result = filter.filter_logs(logs, Some(test_address(50)));
        assert_eq!(result.len(), 2, "Should filter out only private contract logs");

        // Verify correct logs remained
        assert_eq!(result[0].address(), public_contract);
        assert_eq!(result[1].address(), public_contract);
    }

    // ==================== DETAILED RESULT TESTS ====================

    #[test]
    fn test_filter_storage_detailed_denied() {
        let contract = test_address(1);
        let admin = test_address(2);
        let (registry, store) = setup_registry_with_private_contract(contract, admin, false);

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        let slot = U256::from(5);
        let value = B256::from(U256::from(12345));

        let result = filter.filter_storage_detailed(contract, slot, value, None);
        assert!(result.is_denied(), "Should report access as denied");
        assert_eq!(result.value(), B256::ZERO);
    }

    #[test]
    fn test_filter_storage_detailed_allowed() {
        let contract = test_address(1);
        let admin = test_address(2);
        let (registry, store) = setup_registry_with_private_contract(contract, admin, false);

        let filter = PrivacyRpcFilter::new(Some(registry), Some(store));

        // Public slot
        let slot = U256::from(10);
        let value = B256::from(U256::from(12345));

        let result = filter.filter_storage_detailed(contract, slot, value, None);
        assert!(!result.is_denied(), "Should report access as allowed");
        assert_eq!(result.value(), value);
    }

    // ==================== CONFIG TESTS ====================

    #[test]
    fn test_filter_with_custom_config() {
        let contract = test_address(1);
        let admin = test_address(2);
        let (registry, store) = setup_registry_with_private_contract(contract, admin, true);

        // Config that hides storage but not logs
        let config = RpcFilterConfig {
            hide_private_values: true,
            filter_private_logs: false,
        };

        let filter = PrivacyRpcFilter::with_config(Some(registry), Some(store), config);

        // Storage should be filtered
        let slot = U256::from(5);
        let value = B256::from(U256::from(12345));
        let storage_result = filter.filter_storage(contract, slot, value, None);
        assert_eq!(storage_result, B256::ZERO, "Storage should be filtered");

        // Logs should NOT be filtered (even with hide_events=true)
        let logs = vec![create_test_log(contract, U256::from(100))];
        let log_result = filter.filter_logs(logs.clone(), None);
        assert_eq!(log_result.len(), 1, "Logs should not be filtered");
    }

    // ==================== HELPER FUNCTIONS ====================

    fn create_test_log(address: Address, data: U256) -> Log {
        use alloy_primitives::Bytes;
        use alloy_rpc_types_eth::Log as RpcLog;

        RpcLog {
            inner: alloy_primitives::Log {
                address,
                data: alloy_primitives::LogData::new_unchecked(
                    vec![],
                    Bytes::from(data.to_be_bytes::<32>().to_vec()),
                ),
            },
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            block_timestamp: None,
            transaction_hash: Some(B256::ZERO),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        }
    }
}
