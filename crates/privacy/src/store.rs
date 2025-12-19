//! Private State Store
//!
//! In-memory storage for private slot values and authorization entries.
//! Uses `ArcSwapOption` for lock-free reads with atomic updates.

use alloy_primitives::{Address, U256};
use arc_swap::ArcSwapOption;
use derive_more::{Display, From};
use std::{
    collections::HashMap,
    sync::Arc,
};
use tokio::sync::mpsc;

/// Permission constant for read access.
pub const READ: u8 = 0x01;
/// Permission constant for write access.
pub const WRITE: u8 = 0x02;
/// Permission constant for both read and write access.
pub const READ_WRITE: u8 = READ | WRITE;

/// An entry in the private storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateEntry {
    /// The storage value
    pub value: U256,
    /// Who owns this slot
    pub owner: Address,
    /// Block number when last updated
    pub updated_at: u64,
}

/// An authorization entry allowing a delegate to access private storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthEntry {
    /// Permission flags (READ, WRITE, or both)
    pub permissions: u8,
    /// Block number when this authorization expires (0 = never)
    pub expiry: u64,
    /// Block number when this authorization was granted
    pub granted_at: u64,
}

impl AuthEntry {
    /// Create a new authorization entry.
    pub fn new(permissions: u8, expiry: u64, granted_at: u64) -> Self {
        Self {
            permissions,
            expiry,
            granted_at,
        }
    }

    /// Check if this authorization has the given permission.
    pub fn has_permission(&self, permission: u8) -> bool {
        (self.permissions & permission) != 0
    }

    /// Check if this authorization has expired at the given block.
    pub fn is_expired(&self, current_block: u64) -> bool {
        self.expiry != 0 && current_block > self.expiry
    }
}

/// Errors that can occur during store operations.
#[derive(Debug, Display, From, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The caller is not authorized
    #[display("not authorized to access slot")]
    NotAuthorized,

    /// The slot was not found
    #[display("slot not found")]
    NotFound,

    /// Internal error
    #[display("internal store error: {_0}")]
    Internal(String),
}

impl std::error::Error for StoreError {}

/// The immutable state snapshot.
#[derive(Debug, Clone, Default)]
pub struct PrivateState {
    /// Storage: (contract, slot) → PrivateEntry
    pub store: HashMap<(Address, U256), PrivateEntry>,
    /// Authorizations: (contract, slot, delegate) → AuthEntry
    pub authorizations: HashMap<(Address, U256, Address), AuthEntry>,
}

impl PrivateState {
    /// Create a new empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a value from the store.
    pub fn get(&self, contract: Address, slot: U256) -> U256 {
        self.store
            .get(&(contract, slot))
            .map(|e| e.value)
            .unwrap_or(U256::ZERO)
    }

    /// Get the owner of a slot.
    pub fn get_owner(&self, contract: Address, slot: U256) -> Option<Address> {
        self.store.get(&(contract, slot)).map(|e| e.owner)
    }

    /// Check if a caller is authorized to access a slot.
    pub fn is_authorized(
        &self,
        contract: Address,
        slot: U256,
        caller: Address,
        permission: u8,
        current_block: u64,
    ) -> bool {
        // Check 1: Is caller the owner?
        if let Some(entry) = self.store.get(&(contract, slot)) {
            if entry.owner == caller {
                return true;
            }
        }

        // Check 2: Does caller have explicit authorization for this slot?
        if let Some(auth) = self.authorizations.get(&(contract, slot, caller)) {
            if auth.has_permission(permission) && !auth.is_expired(current_block) {
                return true;
            }
        }

        // Check 3: Does caller have wildcard authorization (slot = 0)?
        if let Some(auth) = self.authorizations.get(&(contract, U256::ZERO, caller)) {
            if auth.has_permission(permission) && !auth.is_expired(current_block) {
                return true;
            }
        }

        false
    }
}

/// Update message for the private state.
#[derive(Debug, Clone)]
pub enum PrivateStateUpdate {
    /// Set a storage value
    Set {
        /// Contract address
        contract: Address,
        /// Storage slot
        slot: U256,
        /// New value
        value: U256,
        /// Owner of this slot
        owner: Address,
        /// Current block number
        block: u64,
    },
    /// Grant authorization to a delegate
    Authorize {
        /// Contract address
        contract: Address,
        /// Storage slot (or U256::ZERO for wildcard)
        slot: U256,
        /// Delegate address
        delegate: Address,
        /// Authorization entry
        auth: AuthEntry,
    },
    /// Revoke authorization from a delegate
    Revoke {
        /// Contract address
        contract: Address,
        /// Storage slot
        slot: U256,
        /// Delegate address
        delegate: Address,
    },
    /// Update the current block number
    SetBlock(u64),
}

/// The Private State Store manages private storage values and authorizations.
///
/// Uses lock-free reads via `ArcSwapOption` for high read throughput.
/// Updates are sent via channel to be processed by a background task.
#[derive(Debug)]
pub struct PrivateStateStore {
    /// Lock-free atomic state for read-heavy workloads
    state: Arc<ArcSwapOption<PrivateState>>,
    /// Current block number
    current_block: Arc<std::sync::atomic::AtomicU64>,
    /// Channel for state updates
    update_tx: mpsc::UnboundedSender<PrivateStateUpdate>,
    /// Receiver (held by the processor task)
    update_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<PrivateStateUpdate>>>,
}

impl Clone for PrivateStateStore {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            current_block: Arc::clone(&self.current_block),
            update_tx: self.update_tx.clone(),
            update_rx: Arc::clone(&self.update_rx),
        }
    }
}

impl Default for PrivateStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PrivateStateStore {
    /// Create a new private state store.
    pub fn new() -> Self {
        let (update_tx, update_rx) = mpsc::unbounded_channel();

        Self {
            state: Arc::new(ArcSwapOption::new(Some(Arc::new(PrivateState::new())))),
            current_block: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            update_tx,
            update_rx: Arc::new(tokio::sync::Mutex::new(update_rx)),
        }
    }

    /// Get a value from the private store.
    ///
    /// Returns `U256::ZERO` if the slot doesn't exist.
    pub fn get(&self, contract: Address, slot: U256) -> U256 {
        self.state
            .load()
            .as_ref()
            .map(|s| s.get(contract, slot))
            .unwrap_or(U256::ZERO)
    }

    /// Get the owner of a slot.
    pub fn get_owner(&self, contract: Address, slot: U256) -> Option<Address> {
        self.state
            .load()
            .as_ref()
            .and_then(|s| s.get_owner(contract, slot))
    }

    /// Check if a caller is authorized to access a slot.
    pub fn is_authorized(
        &self,
        contract: Address,
        slot: U256,
        caller: Address,
        permission: u8,
    ) -> bool {
        let current_block = self.current_block.load(std::sync::atomic::Ordering::Relaxed);
        self.state
            .load()
            .as_ref()
            .map(|s| s.is_authorized(contract, slot, caller, permission, current_block))
            .unwrap_or(false)
    }

    /// Get the authorization entry for a specific delegate.
    ///
    /// Returns None if no authorization exists for this (contract, slot, delegate) tuple.
    pub fn get_authorization(
        &self,
        contract: Address,
        slot: U256,
        delegate: Address,
    ) -> Option<AuthEntry> {
        self.state
            .load()
            .as_ref()
            .and_then(|s| s.authorizations.get(&(contract, slot, delegate)).cloned())
    }

    /// Set a value in the private store (synchronous, for use in EVM execution).
    ///
    /// This directly mutates the state - use sparingly.
    /// For async contexts, prefer `update()`.
    pub fn set(&self, contract: Address, slot: U256, value: U256, owner: Address) {
        let block = self.current_block.load(std::sync::atomic::Ordering::Relaxed);

        // Load current state, clone and modify
        let new_state = {
            let current = self.state.load();
            let mut state = current
                .as_ref()
                .map(|s| (**s).clone())
                .unwrap_or_default();

            state.store.insert(
                (contract, slot),
                PrivateEntry {
                    value,
                    owner,
                    updated_at: block,
                },
            );

            Arc::new(state)
        };

        self.state.store(Some(new_state));
    }

    /// Grant authorization to a delegate (synchronous).
    pub fn authorize(&self, contract: Address, slot: U256, delegate: Address, auth: AuthEntry) {
        let new_state = {
            let current = self.state.load();
            let mut state = current
                .as_ref()
                .map(|s| (**s).clone())
                .unwrap_or_default();

            state.authorizations.insert((contract, slot, delegate), auth);

            Arc::new(state)
        };

        self.state.store(Some(new_state));
    }

    /// Revoke authorization from a delegate (synchronous).
    pub fn revoke(&self, contract: Address, slot: U256, delegate: Address) {
        let new_state = {
            let current = self.state.load();
            let mut state = current
                .as_ref()
                .map(|s| (**s).clone())
                .unwrap_or_default();

            state.authorizations.remove(&(contract, slot, delegate));

            Arc::new(state)
        };

        self.state.store(Some(new_state));
    }

    /// Set the current block number.
    pub fn set_block(&self, block: u64) {
        self.current_block
            .store(block, std::sync::atomic::Ordering::Relaxed);
    }

    /// Get the current block number.
    pub fn current_block(&self) -> u64 {
        self.current_block
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Send an update to be processed asynchronously.
    ///
    /// Non-blocking - queues the update for background processing.
    pub fn update(&self, update: PrivateStateUpdate) {
        let _ = self.update_tx.send(update);
    }

    /// Get the number of stored entries.
    pub fn len(&self) -> usize {
        self.state
            .load()
            .as_ref()
            .map(|s| s.store.len())
            .unwrap_or(0)
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the number of authorization entries.
    pub fn auth_count(&self) -> usize {
        self.state
            .load()
            .as_ref()
            .map(|s| s.authorizations.len())
            .unwrap_or(0)
    }

    /// Start the background processor task.
    ///
    /// This task processes updates from the channel and applies them to the state.
    /// Should be spawned as a tokio task.
    pub async fn start_processor(&self) {
        let state = Arc::clone(&self.state);
        let current_block = Arc::clone(&self.current_block);
        let mut rx = self.update_rx.lock().await;

        while let Some(update) = rx.recv().await {
            match update {
                PrivateStateUpdate::Set {
                    contract,
                    slot,
                    value,
                    owner,
                    block,
                } => {
                    let new_state = {
                        let current = state.load();
                        let mut s = current
                            .as_ref()
                            .map(|s| (**s).clone())
                            .unwrap_or_default();

                        s.store.insert(
                            (contract, slot),
                            PrivateEntry {
                                value,
                                owner,
                                updated_at: block,
                            },
                        );

                        Arc::new(s)
                    };
                    state.store(Some(new_state));
                }
                PrivateStateUpdate::Authorize {
                    contract,
                    slot,
                    delegate,
                    auth,
                } => {
                    let new_state = {
                        let current = state.load();
                        let mut s = current
                            .as_ref()
                            .map(|s| (**s).clone())
                            .unwrap_or_default();

                        s.authorizations.insert((contract, slot, delegate), auth);

                        Arc::new(s)
                    };
                    state.store(Some(new_state));
                }
                PrivateStateUpdate::Revoke {
                    contract,
                    slot,
                    delegate,
                } => {
                    let new_state = {
                        let current = state.load();
                        let mut s = current
                            .as_ref()
                            .map(|s| (**s).clone())
                            .unwrap_or_default();

                        s.authorizations.remove(&(contract, slot, delegate));

                        Arc::new(s)
                    };
                    state.store(Some(new_state));
                }
                PrivateStateUpdate::SetBlock(block) => {
                    current_block.store(block, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    #[test]
    fn test_new_store_is_empty() {
        let store = PrivateStateStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_set_and_get() {
        let store = PrivateStateStore::new();
        let contract = test_address(1);
        let slot = U256::from(42);
        let value = U256::from(1000);
        let owner = test_address(10);

        store.set(contract, slot, value, owner);

        assert_eq!(store.get(contract, slot), value);
        assert_eq!(store.get_owner(contract, slot), Some(owner));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn test_get_nonexistent_returns_zero() {
        let store = PrivateStateStore::new();
        let contract = test_address(1);
        let slot = U256::from(42);

        assert_eq!(store.get(contract, slot), U256::ZERO);
        assert_eq!(store.get_owner(contract, slot), None);
    }

    #[test]
    fn test_owner_is_authorized() {
        let store = PrivateStateStore::new();
        let contract = test_address(1);
        let slot = U256::from(42);
        let owner = test_address(10);

        store.set(contract, slot, U256::from(100), owner);

        assert!(store.is_authorized(contract, slot, owner, READ));
        assert!(store.is_authorized(contract, slot, owner, WRITE));
        assert!(store.is_authorized(contract, slot, owner, READ_WRITE));
    }

    #[test]
    fn test_non_owner_not_authorized_by_default() {
        let store = PrivateStateStore::new();
        let contract = test_address(1);
        let slot = U256::from(42);
        let owner = test_address(10);
        let attacker = test_address(99);

        store.set(contract, slot, U256::from(100), owner);

        assert!(!store.is_authorized(contract, slot, attacker, READ));
        assert!(!store.is_authorized(contract, slot, attacker, WRITE));
    }

    #[test]
    fn test_delegate_authorization() {
        let store = PrivateStateStore::new();
        let contract = test_address(1);
        let slot = U256::from(42);
        let owner = test_address(10);
        let delegate = test_address(20);

        store.set(contract, slot, U256::from(100), owner);
        store.authorize(
            contract,
            slot,
            delegate,
            AuthEntry::new(READ, 0, 1),
        );

        assert!(store.is_authorized(contract, slot, delegate, READ));
        assert!(!store.is_authorized(contract, slot, delegate, WRITE));
    }

    #[test]
    fn test_authorization_expiry() {
        let store = PrivateStateStore::new();
        store.set_block(100);

        let contract = test_address(1);
        let slot = U256::from(42);
        let delegate = test_address(20);

        // Grant authorization that expired at block 50
        store.authorize(
            contract,
            slot,
            delegate,
            AuthEntry::new(READ, 50, 1),
        );

        // Should not be authorized (expired)
        assert!(!store.is_authorized(contract, slot, delegate, READ));

        // Grant authorization that expires at block 200
        store.authorize(
            contract,
            slot,
            delegate,
            AuthEntry::new(READ, 200, 1),
        );

        // Should be authorized (not expired)
        assert!(store.is_authorized(contract, slot, delegate, READ));
    }

    #[test]
    fn test_wildcard_authorization() {
        let store = PrivateStateStore::new();
        let contract = test_address(1);
        let owner = test_address(10);
        let delegate = test_address(20);

        // Set up some private data
        store.set(contract, U256::from(1), U256::from(100), owner);
        store.set(contract, U256::from(2), U256::from(200), owner);

        // Authorize delegate for ALL slots (slot = 0)
        store.authorize(
            contract,
            U256::ZERO,
            delegate,
            AuthEntry::new(READ, 0, 1),
        );

        // Should be authorized for any slot
        assert!(store.is_authorized(contract, U256::from(1), delegate, READ));
        assert!(store.is_authorized(contract, U256::from(2), delegate, READ));
        assert!(store.is_authorized(contract, U256::from(999), delegate, READ));
    }

    #[test]
    fn test_revoke_authorization() {
        let store = PrivateStateStore::new();
        let contract = test_address(1);
        let slot = U256::from(42);
        let delegate = test_address(20);

        store.authorize(
            contract,
            slot,
            delegate,
            AuthEntry::new(READ, 0, 1),
        );

        assert!(store.is_authorized(contract, slot, delegate, READ));

        store.revoke(contract, slot, delegate);

        assert!(!store.is_authorized(contract, slot, delegate, READ));
    }

    #[test]
    fn test_overwrite_value() {
        let store = PrivateStateStore::new();
        let contract = test_address(1);
        let slot = U256::from(42);
        let owner = test_address(10);

        store.set(contract, slot, U256::from(100), owner);
        assert_eq!(store.get(contract, slot), U256::from(100));

        store.set(contract, slot, U256::from(200), owner);
        assert_eq!(store.get(contract, slot), U256::from(200));
    }

    #[test]
    fn test_multiple_contracts() {
        let store = PrivateStateStore::new();
        let contract1 = test_address(1);
        let contract2 = test_address(2);
        let slot = U256::from(42);
        let owner = test_address(10);

        store.set(contract1, slot, U256::from(100), owner);
        store.set(contract2, slot, U256::from(200), owner);

        assert_eq!(store.get(contract1, slot), U256::from(100));
        assert_eq!(store.get(contract2, slot), U256::from(200));
    }

    #[test]
    fn test_auth_entry_permissions() {
        let auth = AuthEntry::new(READ, 0, 1);
        assert!(auth.has_permission(READ));
        assert!(!auth.has_permission(WRITE));

        let auth = AuthEntry::new(WRITE, 0, 1);
        assert!(!auth.has_permission(READ));
        assert!(auth.has_permission(WRITE));

        let auth = AuthEntry::new(READ_WRITE, 0, 1);
        assert!(auth.has_permission(READ));
        assert!(auth.has_permission(WRITE));
    }

    #[test]
    fn test_auth_entry_expiry() {
        let auth = AuthEntry::new(READ, 100, 1);
        assert!(!auth.is_expired(50));
        assert!(!auth.is_expired(100));
        assert!(auth.is_expired(101));

        // Never expires
        let auth = AuthEntry::new(READ, 0, 1);
        assert!(!auth.is_expired(0));
        assert!(!auth.is_expired(u64::MAX));
    }
}
