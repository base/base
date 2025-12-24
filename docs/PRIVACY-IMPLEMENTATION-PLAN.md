# Privacy Layer Implementation Plan for node-reth

> **Purpose**: Step-by-step implementation guide for the privacy features described in NODE-RETH-PRIVACY-TDD.md
>
> **Companion Document**: See PRIVACY-TESTING-PLAN.md for the comprehensive testing strategy.
>
> **Last Updated**: After codebase exploration - contains verified integration patterns.

---

## Table of Contents

1. [Implementation Overview](#1-implementation-overview)
2. [Verified Integration Patterns](#2-verified-integration-patterns) *(NEW)*
3. [Prerequisites & Setup](#3-prerequisites--setup)
4. [Phase 1: Core Infrastructure](#4-phase-1-core-infrastructure)
5. [Phase 2: Slot Interception](#5-phase-2-slot-interception)
6. [Phase 3: RPC Filtering](#6-phase-3-rpc-filtering)
7. [Phase 4: Block Publishing](#7-phase-4-block-publishing)
8. [Phase 5: Persistence & Authorization](#8-phase-5-persistence--authorization)
9. [Phase 6: Solidity Library](#9-phase-6-solidity-library)
10. [Critical Path & Dependencies](#10-critical-path--dependencies)
11. [Risk Assessment](#11-risk-assessment)
12. [Phase Testing Checkpoints](#12-phase-testing-checkpoints) *(NEW)*

---

## 1. Implementation Overview

### What We're Building

A dual-store privacy architecture that routes SLOAD/SSTORE operations to either:
- **Public store (triedb)**: Standard Ethereum state trie
- **Private store (TEE-local HashMap)**: Encrypted, access-controlled storage

### Crate Structure

```
crates/
├── privacy/                          # NEW CRATE
│   ├── src/
│   │   ├── lib.rs                   # Public API
│   │   ├── registry.rs              # PrivacyRegistry, SlotConfig
│   │   ├── store.rs                 # PrivateStateStore (HashMap)
│   │   ├── classification.rs        # Slot classification logic
│   │   ├── precompiles/
│   │   │   ├── mod.rs
│   │   │   ├── registry.rs          # 0x200 precompile
│   │   │   └── auth.rs              # 0x201 precompile
│   │   ├── host.rs                  # PrivacyAwareHost wrapper
│   │   ├── persistence.rs           # Sealed storage (placeholder)
│   │   └── metrics.rs               # Privacy operation metrics
│   └── Cargo.toml
│
├── runner/                           # MODIFY
│   └── src/
│      └── extensions/
│         └── privacy.rs             # PrivacyExtension integration
│
├── flashblocks/                      # MODIFY
│   └── src/
│      └── processor.rs              # Private state filtering
│
└── rpc/                              # MODIFY
   └── src/
      └── eth/
         ├── storage.rs              # Filter eth_getStorageAt
         └── logs.rs                 # Filter eth_getLogs
```

### Key Integration Points

| Component | File | Modification |
|-----------|------|--------------|
| EVM Execution | `flashblocks/src/processor.rs` | Wrap Host with PrivacyAwareHost |
| RPC Layer | `rpc/src/eth/rpc.rs` | Filter private slots in responses |
| Node Runner | `runner/src/runner.rs` | Add PrivacyExtension |
| Precompiles | (new) `privacy/src/precompiles/` | Register 0x200, 0x201 |

---

## 2. Verified Integration Patterns

> **This section contains concrete patterns discovered during codebase exploration.**
> These are the exact integration points and APIs we'll use.

### 2.1 revm Version & Database Trait (NOT Host)

**Key Discovery**: In revm 31.0.2, storage operations are intercepted via the `Database` trait, NOT a `Host` trait wrapper.

**Version**: `revm = "31.0.2"` (from workspace Cargo.toml line 98)

The `Database` trait has these key methods for storage interception:

```rust
pub trait Database {
    type Error;

    /// Get storage value of address at index
    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error>;

    /// Get basic account information (balance, nonce, code)
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error>;

    /// Get account code by hash
    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error>;

    /// Get block hash by block number
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error>;
}
```

**Implication**: We implement `PrivacyAwareDatabase` wrapping `StateProviderDatabase`, NOT a Host wrapper.

### 2.2 Exact Integration Point in processor.rs

**File**: `crates/flashblocks/src/processor.rs`
**Lines**: 291-301

```rust
// Line 291: Get EVM configuration for Optimism chain
let evm_config = OpEvmConfig::optimism(self.client.chain_spec());

// Lines 292-294: Get state provider from canonical block
let state_provider =
    self.client.state_by_block_number_or_tag(BlockNumberOrTag::Number(canonical_block))?;
let state_provider_db = StateProviderDatabase::new(state_provider);

// Line 295: Build State with bundle update enabled
let state = State::builder()
    .with_database(state_provider_db)  // <-- WRAP HERE
    .with_bundle_update()
    .build();

// Lines 298-301: Wrap in CacheDB for transaction-level caching
let mut db = match &prev_pending_blocks {
    Some(pending_blocks) => CacheDB {
        cache: pending_blocks.get_db_cache(),
        db: state
    },
    None => CacheDB::new(state),
};
```

**Our Integration**: Replace line 294 to wrap `StateProviderDatabase`:

```rust
let state_provider_db = StateProviderDatabase::new(state_provider);

// NEW: Wrap with privacy-aware database
let privacy_db = PrivacyAwareDatabase::new(
    state_provider_db,
    self.privacy_registry.clone(),
    self.private_store.clone(),
);

let state = State::builder()
    .with_database(privacy_db)  // Use privacy wrapper
    .with_bundle_update()
    .build();
```

### 2.3 Precompile Registration Pattern

**Discovery**: Reth uses `ConfigureEvm` trait with handler registration.

**Pattern from AlphaNet/SUAVE**:

```rust
use reth_node_api::ConfigureEvm;
use revm::{ContextPrecompiles, Database, Evm, EvmBuilder, PrecompileSpecId};
use std::sync::Arc;

/// Custom precompile addresses
pub const PRIVACY_REGISTRY_ADDRESS: Address = address!("0000000000000000000000000000000000000200");
pub const PRIVACY_AUTH_ADDRESS: Address = address!("0000000000000000000000000000000000000201");

/// Privacy-aware EVM configuration
#[derive(Debug, Clone)]
pub struct PrivacyEvmConfig {
    inner: OpEvmConfig,
    registry: Arc<PrivacyRegistry>,
    store: Arc<PrivateStateStore>,
}

impl ConfigureEvm for PrivacyEvmConfig {
    type DefaultExternalContext<'a> = ();

    fn evm<DB: Database>(&self, db: DB) -> Evm<'_, (), DB> {
        let registry = self.registry.clone();
        let store = self.store.clone();

        EvmBuilder::default()
            .with_db(db)
            .append_handler_register(move |handler| {
                let spec_id = handler.cfg.spec_id;

                // Register custom precompiles
                handler.pre_execution.load_precompiles = Arc::new(move || {
                    let mut precompiles: ContextPrecompiles<DB> =
                        ContextPrecompiles::new(PrecompileSpecId::from_spec_id(spec_id));

                    // Add privacy precompiles
                    precompiles.extend([
                        privacy_registry_precompile(registry.clone()),
                        privacy_auth_precompile(store.clone()),
                    ]);

                    precompiles
                });
            })
            .build()
    }
}
```

### 2.4 Shared State Management (OnceCell Pattern)

**Discovery**: node-reth uses `Arc<OnceCell<Arc<T>>>` for cross-extension state sharing.

**File**: `crates/runner/src/extensions/types.rs`

```rust
// Existing pattern for FlashblocksState
pub type FlashblocksCell = Arc<OnceCell<Arc<FlashblocksState<OpProvider>>>>;

// NEW: Add for privacy state
pub type PrivacyRegistryCell = Arc<OnceCell<Arc<PrivacyRegistry>>>;
pub type PrivateStateStoreCell = Arc<OnceCell<Arc<PrivateStateStore>>>;
```

**Usage in BaseNodeConfig** (`crates/runner/src/config.rs`):

```rust
pub struct BaseNodeConfig {
    pub rollup_args: RollupArgs,
    pub flashblocks: FlashblocksConfig,
    pub flashblocks_cell: FlashblocksCell,
    pub tracing: TracingConfig,
    pub metering: MeteringConfig,

    // NEW: Privacy configuration
    pub privacy: PrivacyConfig,
    pub privacy_registry_cell: PrivacyRegistryCell,
    pub private_store_cell: PrivateStateStoreCell,
}
```

**Extension Pattern** (follows `FlashblocksCanonExtension`):

```rust
pub struct PrivacyExtension {
    registry_cell: PrivacyRegistryCell,
    store_cell: PrivateStateStoreCell,
}

impl BaseNodeExtension for PrivacyExtension {
    fn apply(&self, builder: OpBuilder) -> OpBuilder {
        let registry_cell = self.registry_cell.clone();
        let store_cell = self.store_cell.clone();

        builder.install_exex("privacy-state", move |ctx| {
            let registry = registry_cell
                .get_or_init(|| Arc::new(PrivacyRegistry::new()))
                .clone();
            let store = store_cell
                .get_or_init(|| Arc::new(PrivateStateStore::new()))
                .clone();

            async move {
                // Handle canonical chain notifications for state updates
                while let Some(notification) = ctx.notifications.try_next().await? {
                    if let Some(committed) = notification.committed_chain() {
                        for block in committed.blocks() {
                            // Process privacy-related transactions in block
                            process_privacy_transactions(&registry, &store, block);
                        }
                    }
                }
                Ok(())
            }
        })
    }
}
```

### 2.5 State Persistence via ArcSwap

**Discovery**: Use `ArcSwapOption` for lock-free state updates (like FlashblocksState).

```rust
use arc_swap::ArcSwapOption;

pub struct PrivateStateStore {
    // Lock-free atomic state for read-heavy workloads
    state: Arc<ArcSwapOption<PrivateState>>,

    // Channel for state updates (processed by background task)
    update_tx: mpsc::UnboundedSender<PrivateStateUpdate>,
}

pub struct PrivateState {
    store: HashMap<(Address, U256), PrivateEntry>,
    authorizations: HashMap<(Address, U256, Address), AuthEntry>,
    slot_owners: HashMap<(Address, U256), Address>,
}

impl PrivateStateStore {
    pub fn get(&self, contract: Address, slot: U256) -> U256 {
        self.state
            .load()
            .as_ref()
            .and_then(|s| s.store.get(&(contract, slot)))
            .map(|e| e.value)
            .unwrap_or(U256::ZERO)
    }

    pub fn update(&self, update: PrivateStateUpdate) {
        // Non-blocking: send to background processor
        let _ = self.update_tx.send(update);
    }
}
```

### 2.6 Transaction Execution Flow

**Verified execution pipeline** (processor.rs lines 390-517):

```
1. evm_config.evm_with_env(db, evm_env)  → Create EVM instance
2. evm.transact(recovered_transaction)    → Execute (calls Database::storage internally)
3. ResultAndState { state, result }       → Get state changes
4. evm.db_mut().commit(state)             → Commit to CacheDB
5. evm.into_db()                          → Extract database for next tx
```

**Key insight**: `Database::storage()` is called by the EVM interpreter during SLOAD opcode execution. We intercept there.

### 2.7 Mapping Key Capture Strategy

**Challenge**: Can't intercept `keccak256` at Database level (it's computed in interpreter).

**Solution**: Track writes and infer ownership from call context:

```rust
impl<DB: Database> Database for PrivacyAwareDatabase<DB> {
    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        // Check classification
        match self.classify_slot(address, index) {
            SlotClassification::Public => self.inner.storage(address, index),
            SlotClassification::Private { .. } => {
                // Read from private store
                Ok(self.private_store.get(address, index))
            }
            SlotClassification::Unknown => {
                // Conservative: treat as private, return from private or zero
                Ok(self.private_store.get(address, index))
            }
        }
    }
}
```

**For writes**: Intercept in `commit()` method when state changes are finalized:

```rust
impl<DB: Database> DatabaseCommit for PrivacyAwareDatabase<DB> {
    fn commit(&mut self, changes: HashMap<Address, Account>) {
        for (address, account) in changes {
            for (slot, value) in account.storage {
                match self.classify_slot(address, slot) {
                    SlotClassification::Public => {
                        // Let inner database handle it
                    }
                    SlotClassification::Private { owner } => {
                        // Redirect to private store
                        self.private_store.set(address, slot, value.present_value, owner);
                        // DON'T commit to inner (remove from changes)
                    }
                }
            }
        }
        // Commit remaining public changes
        self.inner.commit(filtered_changes);
    }
}
```

---

## 3. Prerequisites & Setup

### Development Environment

```bash
# Clone and enter workspace
git clone https://github.com/base/node-reth.git
cd node-reth

# Ensure Rust toolchain is correct
rustup show  # Should be 1.88+

# Install development tools
cargo install cargo-nextest
cargo install just

# Build the project
just build

# Run tests to verify setup
just test
```

### Create Privacy Crate Scaffold

```bash
# Create the new crate
mkdir -p crates/privacy/src/precompiles

# Add to workspace Cargo.toml
# [workspace.members] should include "crates/privacy"
```

**Initial `crates/privacy/Cargo.toml`:**

```toml
[package]
name = "base-reth-privacy"
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
license = "MIT"

[dependencies]
# Core types
alloy-primitives.workspace = true
alloy-sol-types.workspace = true

# Execution
revm.workspace = true
reth-revm.workspace = true
reth-primitives.workspace = true

# Async
tokio = { workspace = true, features = ["sync"] }

# Serialization (for persistence)
serde = { workspace = true, features = ["derive"] }
bincode.workspace = true

# Logging/metrics
tracing.workspace = true
metrics.workspace = true

# Error handling
eyre.workspace = true
thiserror.workspace = true

[dev-dependencies]
base-reth-test-utils.workspace = true
```

---

## 3. Phase 1: Core Infrastructure

### 3.1 Data Types (`privacy/src/lib.rs`)

```rust
//! Privacy Layer for Base node-reth
//!
//! Implements dual-store architecture for private contract storage.

pub mod classification;
pub mod host;
pub mod precompiles;
pub mod registry;
pub mod store;

// Re-exports
pub use classification::{SlotClassification, classify_slot};
pub use host::PrivacyAwareHost;
pub use registry::{PrivacyRegistry, PrivateContractConfig, SlotConfig, SlotType, OwnershipType};
pub use store::{PrivateStateStore, PrivateEntry, AuthEntry};
```

### 3.2 Registry Types (`privacy/src/registry.rs`)

**Tasks:**
- [ ] Define `PrivateContractConfig` struct
- [ ] Define `SlotConfig` struct with `SlotType` and `OwnershipType` enums
- [ ] Implement `PrivacyRegistry` with in-memory HashMap storage
- [ ] Add registration/query methods
- [ ] Add thread-safe access via `Arc<RwLock<>>`

**Key Implementation:**

```rust
use alloy_primitives::{Address, U256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub struct PrivateContractConfig {
    pub address: Address,
    pub admin: Address,
    pub slots: Vec<SlotConfig>,
    pub registered_at: u64,
}

#[derive(Debug, Clone)]
pub struct SlotConfig {
    pub base_slot: U256,
    pub slot_type: SlotType,
    pub ownership: OwnershipType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotType {
    Simple,
    Mapping,
    NestedMapping,
    MappingToStruct { struct_size: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipType {
    Contract,
    MappingKey,
    OuterKey,
    InnerKey,
    FixedOwner(Address),
}

#[derive(Clone)]
pub struct PrivacyRegistry {
    configs: Arc<RwLock<HashMap<Address, PrivateContractConfig>>>,
    // Track computed slot → owner mappings
    mapping_owners: Arc<RwLock<HashMap<(Address, U256), Address>>>,
}

impl PrivacyRegistry {
    pub fn new() -> Self { /* ... */ }

    pub fn register(&self, config: PrivateContractConfig) -> Result<(), RegistryError> { /* ... */ }

    pub fn is_registered(&self, address: &Address) -> bool { /* ... */ }

    pub fn get_config(&self, address: &Address) -> Option<PrivateContractConfig> { /* ... */ }

    pub fn record_slot_owner(&self, contract: Address, slot: U256, owner: Address) { /* ... */ }
}
```

### 3.3 Private State Store (`privacy/src/store.rs`)

**Tasks:**
- [ ] Implement `PrivateStateStore` with HashMap<(Address, U256), PrivateEntry>
- [ ] Implement `PrivateEntry` with value, owner, timestamp
- [ ] Implement `AuthEntry` for delegations
- [ ] Add get/set/is_authorized methods
- [ ] Thread-safe access

**Key Implementation:**

```rust
use alloy_primitives::{Address, U256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub const READ: u8 = 0x01;
pub const WRITE: u8 = 0x02;

#[derive(Debug, Clone)]
pub struct PrivateEntry {
    pub value: U256,
    pub owner: Address,
    pub updated_at: u64,
}

#[derive(Debug, Clone)]
pub struct AuthEntry {
    pub permissions: u8,
    pub expiry: u64,
    pub granted_at: u64,
}

pub struct PrivateStateStore {
    store: Arc<RwLock<HashMap<(Address, U256), PrivateEntry>>>,
    authorizations: Arc<RwLock<HashMap<(Address, U256, Address), AuthEntry>>>,
    current_block: u64,
}

impl PrivateStateStore {
    pub fn new() -> Self { /* ... */ }

    pub fn get(&self, contract: Address, slot: U256) -> U256 { /* ... */ }

    pub fn set(&mut self, contract: Address, slot: U256, value: U256, owner: Address) { /* ... */ }

    pub fn is_authorized(&self, contract: Address, slot: U256, caller: Address, permission: u8) -> bool {
        // Check 1: Is caller the owner?
        // Check 2: Explicit authorization?
        // Check 3: Wildcard authorization (slot = 0)?
    }

    pub fn authorize(&mut self, contract: Address, slot: U256, delegate: Address, auth: AuthEntry) { /* ... */ }

    pub fn revoke(&mut self, contract: Address, slot: U256, delegate: Address) { /* ... */ }
}
```

### 3.4 Slot Classification (`privacy/src/classification.rs`)

**Tasks:**
- [ ] Implement `classify_slot()` function
- [ ] Handle simple slots
- [ ] Handle mapping slot derivation tracking
- [ ] Return `SlotClassification::Private { owner }` or `SlotClassification::Public`

**Key Implementation:**

```rust
use alloy_primitives::{Address, U256};

pub enum SlotClassification {
    Public,
    Private { owner: Address },
}

pub fn classify_slot(
    registry: &PrivacyRegistry,
    contract: Address,
    slot: U256,
) -> SlotClassification {
    let config = match registry.get_config(&contract) {
        Some(c) => c,
        None => return SlotClassification::Public,
    };

    for slot_config in &config.slots {
        match slot_config.slot_type {
            SlotType::Simple if slot == slot_config.base_slot => {
                let owner = resolve_owner(&slot_config.ownership, None, None, contract);
                return SlotClassification::Private { owner };
            }
            SlotType::Mapping | SlotType::NestedMapping => {
                // Check if this slot is in our mapping_owners cache
                if let Some(owner) = registry.get_slot_owner(contract, slot) {
                    return SlotClassification::Private { owner };
                }
            }
            _ => {}
        }
    }

    SlotClassification::Public
}
```

### 3.5 Unit Tests for Phase 1

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_registration() {
        let registry = PrivacyRegistry::new();
        let config = PrivateContractConfig { /* ... */ };
        registry.register(config).unwrap();
        assert!(registry.is_registered(&config.address));
    }

    #[test]
    fn test_private_store_get_set() {
        let mut store = PrivateStateStore::new();
        let contract = Address::ZERO;
        let slot = U256::from(1);
        let value = U256::from(100);
        let owner = Address::random();

        store.set(contract, slot, value, owner);
        assert_eq!(store.get(contract, slot), value);
    }

    #[test]
    fn test_authorization_check() {
        let mut store = PrivateStateStore::new();
        // ... setup and test authorization logic
    }
}
```

---

## 4. Phase 2: Slot Interception

### 4.1 Privacy-Aware Host Wrapper (`privacy/src/host.rs`)

**Tasks:**
- [ ] Create `PrivacyAwareHost<H>` wrapper struct
- [ ] Implement `Host` trait for the wrapper
- [ ] Intercept `sload()` and `sstore()` methods
- [ ] Route to private/public store based on classification
- [ ] Intercept `keccak256()` for mapping key capture

**Key Implementation:**

```rust
use revm::{Database, Host, SStoreResult, SloadResult};
use alloy_primitives::{Address, U256, B256, keccak256};

pub struct PrivacyAwareHost<H: Host> {
    inner: H,
    registry: PrivacyRegistry,
    private_store: PrivateStateStore,
    pending_mapping_key: Option<MappingKeyInfo>,
    current_contract: Address,
}

#[derive(Debug, Clone)]
struct MappingKeyInfo {
    key: Address,
    base_slot: U256,
    computed_slot: U256,
}

impl<H: Host> PrivacyAwareHost<H> {
    pub fn new(inner: H, registry: PrivacyRegistry, private_store: PrivateStateStore) -> Self {
        Self {
            inner,
            registry,
            private_store,
            pending_mapping_key: None,
            current_contract: Address::ZERO,
        }
    }
}

impl<H: Host> Host for PrivacyAwareHost<H> {
    // Delegate most methods to inner host

    fn sload(&mut self, address: Address, slot: U256) -> Option<SloadResult> {
        match classify_slot(&self.registry, address, slot) {
            SlotClassification::Public => self.inner.sload(address, slot),
            SlotClassification::Private { owner: _ } => {
                let value = self.private_store.get(address, slot);
                Some(SloadResult { value, is_cold: false })
            }
        }
    }

    fn sstore(&mut self, address: Address, slot: U256, value: U256) -> Option<SStoreResult> {
        // Check if we have a pending mapping key from keccak256
        if let Some(key_info) = self.pending_mapping_key.take() {
            if key_info.computed_slot == slot {
                // This is a write to a mapped slot
                if let Some(config) = self.registry.get_config(&address) {
                    if let Some(slot_config) = config.slots.iter()
                        .find(|s| s.base_slot == key_info.base_slot)
                    {
                        let owner = resolve_owner_from_key(&slot_config.ownership, key_info.key);
                        self.registry.record_slot_owner(address, slot, owner);
                        self.private_store.set(address, slot, value, owner);
                        return Some(SStoreResult::default()); // Adjust for actual revm return
                    }
                }
            }
        }

        match classify_slot(&self.registry, address, slot) {
            SlotClassification::Public => self.inner.sstore(address, slot, value),
            SlotClassification::Private { owner } => {
                self.private_store.set(address, slot, value, owner);
                Some(SStoreResult::default())
            }
        }
    }

    // SHA3/KECCAK256 interception for mapping key capture
    fn keccak256(&mut self, data: &[u8]) -> B256 {
        let hash = keccak256(data);

        // Detect mapping slot computation: keccak256(abi.encode(key, base_slot))
        // Size = 64 bytes (32 byte address padded + 32 byte slot)
        if data.len() == 64 {
            let potential_key = Address::from_slice(&data[12..32]);
            let base_slot = U256::from_be_slice(&data[32..64]);

            self.pending_mapping_key = Some(MappingKeyInfo {
                key: potential_key,
                base_slot,
                computed_slot: U256::from_be_bytes(hash.0),
            });
        }

        hash
    }
}
```

### 4.2 Integration with Flashblocks Processor

**File:** `crates/flashblocks/src/processor.rs`

**Tasks:**
- [ ] Add `PrivacyRegistry` and `PrivateStateStore` to `StateProcessor`
- [ ] Wrap the execution Host with `PrivacyAwareHost` before transaction execution
- [ ] Pass through privacy state to block builder

**Modification Points:**

```rust
// In StateProcessor struct, add:
privacy_registry: PrivacyRegistry,
private_store: PrivateStateStore,

// In execute_transaction or equivalent:
let privacy_host = PrivacyAwareHost::new(
    base_host,
    self.privacy_registry.clone(),
    self.private_store.clone(),
);
// Use privacy_host for EVM execution
```

### 4.3 Unit Tests for Phase 2

```rust
#[tokio::test]
async fn test_sload_routes_to_private_store() {
    // Setup registry with a registered contract
    // Execute SLOAD on a private slot
    // Verify value comes from private store, not triedb
}

#[tokio::test]
async fn test_sstore_routes_to_private_store() {
    // Setup registry with a registered contract
    // Execute SSTORE on a private slot
    // Verify value is in private store, NOT in triedb
}

#[tokio::test]
async fn test_mapping_key_capture() {
    // Execute a mapping write (balances[alice] = 100)
    // Verify the slot owner was captured as alice
}

#[tokio::test]
async fn test_public_slots_unchanged() {
    // Verify non-private slots still route to triedb
}
```

---

## 5. Phase 3: RPC Filtering

### 5.1 Filter `eth_getStorageAt`

**File:** `crates/rpc/src/eth/rpc.rs`

**Tasks:**
- [ ] Check slot classification before returning
- [ ] For private slots, check caller authorization
- [ ] Return `U256::ZERO` for unauthorized queries
- [ ] Pass through to triedb for public slots

**Implementation:**

```rust
async fn eth_get_storage_at(
    &self,
    address: Address,
    slot: U256,
    block: BlockId,
) -> RpcResult<U256> {
    match classify_slot(&self.privacy_registry, address, slot) {
        SlotClassification::Public => {
            // Standard path
            self.inner.get_storage_at(address, slot, block).await
        }
        SlotClassification::Private { owner: _ } => {
            // For now, return zero for all external queries
            // Future: support authenticated queries
            Ok(U256::ZERO)
        }
    }
}
```

### 5.2 Filter `eth_call` (Future Enhancement)

For `eth_call`, contracts can read all private storage during execution (needed for logic). The filtering happens on response analysis—this is primarily a contract design concern.

### 5.3 Filter `eth_getLogs`

**File:** `crates/rpc/src/eth/rpc.rs` or new `logs.rs`

**Tasks:**
- [ ] Check if log originates from a private contract
- [ ] Filter logs based on per-contract event privacy config
- [ ] Return filtered log set

**Implementation:**

```rust
async fn eth_get_logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
    let logs = self.inner.get_logs(filter).await?;

    let filtered: Vec<Log> = logs
        .into_iter()
        .filter(|log| {
            if let Some(config) = self.privacy_registry.get_config(&log.address) {
                // Check if events should be hidden
                !config.hide_events
            } else {
                true // Not a private contract, include
            }
        })
        .collect();

    Ok(filtered)
}
```

### 5.3 RPC Extension Module

**File:** Create `crates/rpc/src/privacy_rpc.rs`

**Tasks:**
- [ ] Add privacy-specific RPC methods (optional)
- [ ] `privacy_isRegistered(address)` → bool
- [ ] `privacy_getConfig(address)` → config (for authorized callers)

---

## 6. Phase 4: Block Publishing

### 6.1 State Diff Filtering

**File:** Modify block builder or add filtering layer

**Tasks:**
- [ ] Before publishing state diff to L1/DA, filter private storage changes
- [ ] Private slots should NOT appear in published state root
- [ ] Optionally include private state commitment for future exit mechanism

**Implementation:**

```rust
pub fn filter_state_diff(
    state_diff: StateDiff,
    registry: &PrivacyRegistry,
) -> StateDiff {
    let mut filtered = state_diff.clone();

    filtered.storage_changes.retain(|(contract, slot, _value)| {
        match classify_slot(registry, *contract, *slot) {
            SlotClassification::Public => true,
            SlotClassification::Private { .. } => false,
        }
    });

    filtered
}
```

### 6.2 Receipt Filtering

**Tasks:**
- [ ] Filter logs from transaction receipts for private contracts
- [ ] Configurable per-contract (some may want events visible)

### 6.3 Private State Commitment (Optional)

**Tasks:**
- [ ] Compute Merkle root of private state periodically
- [ ] Include commitment in block for future force-exit mechanism
- [ ] Design commitment structure for proving ownership

---

## 7. Phase 5: Persistence & Authorization

### 7.1 Privacy Registry Precompile (0x200)

**File:** `crates/privacy/src/precompiles/registry.rs`

**Tasks:**
- [ ] Implement precompile at address `0x0000000000000000000000000000000000000200`
- [ ] Handle `register()`, `updateSlots()`, `transferAdmin()` calls
- [ ] Handle `isRegistered()`, `getConfig()` view calls
- [ ] Handle `registerExternal()` (governance-only)
- [ ] Verify caller authorization (deployer/admin only)

**ABI Encoding:**

```rust
use alloy_sol_types::sol;

sol! {
    function register(address contract_, SlotConfig[] slots) external;
    function updateSlots(address contract_, SlotConfig[] slots) external;
    function transferAdmin(address contract_, address newAdmin) external;
    function isRegistered(address contract_) external view returns (bool);
    function getConfig(address contract_) external view returns (PrivateContractConfig);
    function registerExternal(address contract_, SlotConfig[] slots) external;

    struct SlotConfig {
        uint256 baseSlot;
        SlotType slotType;
        OwnershipType ownership;
    }

    enum SlotType { Simple, Mapping, NestedMapping, MappingToStruct }
    enum OwnershipType { Contract, MappingKey, OuterKey, InnerKey, FixedOwner }
}
```

**Precompile Implementation:**

```rust
use reth_revm::precompile::{Precompile, PrecompileResult};

pub struct PrivacyRegistryPrecompile {
    registry: PrivacyRegistry,
}

impl Precompile for PrivacyRegistryPrecompile {
    fn run(&self, input: &[u8], gas_limit: u64, context: &Context) -> PrecompileResult {
        // Decode function selector (first 4 bytes)
        let selector = &input[..4];
        let data = &input[4..];

        match selector {
            // register(address, SlotConfig[])
            [0x..] => self.handle_register(data, context),
            // isRegistered(address)
            [0x..] => self.handle_is_registered(data),
            // ... other functions
            _ => Err(PrecompileError::InvalidInput),
        }
    }
}
```

### 7.2 Privacy Auth Precompile (0x201)

**File:** `crates/privacy/src/precompiles/auth.rs`

**Tasks:**
- [ ] Implement precompile at address `0x0000000000000000000000000000000000000201`
- [ ] Handle `authorize()`, `revoke()`, `isAuthorized()` calls
- [ ] Verify caller is the slot owner

### 7.3 Sealed Storage (TEE Persistence)

**File:** `crates/privacy/src/persistence.rs`

**Tasks:**
- [ ] Define `SealedStorage` trait for TEE integration
- [ ] Implement `persist()` for periodic state snapshots
- [ ] Implement `recover()` for startup state restoration
- [ ] Use placeholder implementation for non-TEE development

**Implementation:**

```rust
pub trait SealedStorage {
    fn seal_and_store(&self, key: &str, data: &[u8]) -> Result<(), Error>;
    fn load(&self, key: &str) -> Result<Vec<u8>, Error>;
    fn unseal(&self, data: &[u8]) -> Result<Vec<u8>, Error>;
}

// Development placeholder
pub struct InMemorySealedStorage {
    data: HashMap<String, Vec<u8>>,
}

impl SealedStorage for InMemorySealedStorage {
    fn seal_and_store(&self, key: &str, data: &[u8]) -> Result<(), Error> {
        // No actual sealing in dev mode
        self.data.insert(key.to_string(), data.to_vec());
        Ok(())
    }
    // ...
}
```

### 7.4 Register Precompiles with Node

**File:** `crates/runner/src/extensions/privacy.rs`

**Tasks:**
- [ ] Create `PrivacyExtension` implementing `BaseNodeExtension`
- [ ] Register 0x200 and 0x201 precompiles
- [ ] Initialize `PrivacyRegistry` and `PrivateStateStore`
- [ ] Wire into node runner

---

## 8. Phase 6: Solidity Library

### 8.1 Create npm Package Structure

```
packages/privacy/
├── package.json
├── src/
│   ├── PrivacyEnabled.sol
│   ├── IPrivacyRegistry.sol
│   ├── IPrivacyAuth.sol
│   └── tokens/
│       ├── PrivateERC20.sol
│       └── PrivateERC721.sol
└── README.md
```

### 8.2 Implement Contracts

**Tasks:**
- [ ] `IPrivacyRegistry.sol`: Interface for 0x200 precompile
- [ ] `IPrivacyAuth.sol`: Interface for 0x201 precompile
- [ ] `PrivacyEnabled.sol`: Base contract with fluent helpers
- [ ] `PrivateERC20.sol`: Ready-made private ERC20
- [ ] `PrivateERC721.sol`: Ready-made private ERC721

### 8.3 Test Contracts

**Tasks:**
- [ ] Unit tests for each contract using Foundry
- [ ] Integration tests deploying to local node

---

## 9. Critical Path & Dependencies

```
Phase 1 (Core Infrastructure)
    │
    ├── registry.rs ─────┐
    ├── store.rs ────────┼──► Phase 2 (Slot Interception)
    └── classification.rs┘           │
                                     │
                                     ▼
                          Phase 3 (RPC Filtering)
                                     │
                                     ▼
                          Phase 4 (Block Publishing)
                                     │
                                     ▼
                          Phase 5 (Persistence/Auth)
                                     │
                                     ▼
                          Phase 6 (Solidity Library)
```

### Parallel Work Streams

| Stream | Owner | Phases |
|--------|-------|--------|
| Core Rust | Backend Dev | 1, 2, 4, 5 |
| RPC Layer | RPC Dev | 3 |
| Solidity | Smart Contract Dev | 6 |
| Testing | QA/All | Continuous |

---

## 10. Risk Assessment

### High Risk

| Risk | Impact | Mitigation |
|------|--------|------------|
| SLOAD/SSTORE hook performance | Slower execution | Benchmark early, optimize hot paths |
| Mapping key capture missing cases | Privacy leaks | Extensive testing, fuzzing |
| State consistency (private vs public) | Data corruption | Transaction-level atomicity |

### Medium Risk

| Risk | Impact | Mitigation |
|------|--------|------------|
| Precompile gas costs | Poor UX | Benchmark, optimize |
| RPC backward compatibility | Breaking clients | Careful API design |
| TEE integration complexity | Delayed persistence | Start with in-memory |

### Low Risk

| Risk | Impact | Mitigation |
|------|--------|------------|
| Solidity library adoption | Low developer interest | Good docs, examples |
| Event filtering edge cases | Some events leak | Per-contract config |

---

## Appendix: File-by-File Changes

| File | Change Type | Description |
|------|-------------|-------------|
| `Cargo.toml` (workspace) | Add | `crates/privacy` to members |
| `crates/privacy/*` | New | Entire privacy crate |
| `crates/runner/src/runner.rs` | Modify | Add PrivacyExtension |
| `crates/runner/Cargo.toml` | Modify | Add base-reth-privacy dependency |
| `crates/flashblocks/src/processor.rs` | Modify | Wrap Host with PrivacyAwareHost |
| `crates/flashblocks/Cargo.toml` | Modify | Add base-reth-privacy dependency |
| `crates/rpc/src/eth/rpc.rs` | Modify | Filter eth_getStorageAt, eth_getLogs |
| `crates/rpc/Cargo.toml` | Modify | Add base-reth-privacy dependency |
| `crates/test-utils/` | Modify | Add privacy testing helpers |
| `packages/privacy/` | New | Solidity library |

---

## 12. Phase Testing Checkpoints

> **Each phase has specific validation criteria before proceeding to the next.**

### Phase 1 Checkpoint: Core Infrastructure

**Validation Tests:**

```bash
# Run after implementing registry.rs, store.rs, classification.rs
cargo test -p base-reth-privacy

# Expected: All unit tests pass
# - test_registry_registration
# - test_private_store_get_set
# - test_authorization_check
# - test_slot_classification
```

**Manual Verification:**
```rust
// Quick REPL-style test
let registry = PrivacyRegistry::new();
let config = PrivateContractConfig { /* ERC20 config */ };
registry.register(config).unwrap();
assert!(registry.is_registered(&config.address));

let mut store = PrivateStateStore::new();
store.set(addr, slot, U256::from(100), owner);
assert_eq!(store.get(addr, slot), U256::from(100));
```

**Exit Criteria:**
- [ ] All Phase 1 unit tests pass
- [ ] Registry can register/query contracts
- [ ] Store can get/set/authorize
- [ ] Classification returns correct Public/Private

---

### Phase 2 Checkpoint: Slot Interception

**Validation Tests:**

```bash
# Integration test with mock database
cargo test -p base-reth-privacy --test database_integration

# Expected tests pass:
# - test_sload_routes_to_private_store
# - test_sstore_routes_to_private_store
# - test_public_slots_unchanged
# - test_commit_filters_private_changes
```

**Integration Test with TestHarness:**

```rust
#[tokio::test]
async fn test_privacy_database_in_execution() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;

    // Deploy a contract that we've registered as private
    let contract = deploy_simple_storage(&harness).await?;

    // Register it for privacy (slot 0 = private)
    register_for_privacy(&harness, contract, vec![
        SlotConfig { base_slot: U256::ZERO, slot_type: SlotType::Simple, ... }
    ]).await?;

    // Write to slot 0 (private)
    set_storage(&harness, contract, U256::ZERO, U256::from(42)).await?;

    // Verify: direct storage query returns 0
    let public_value = harness.provider()
        .get_storage_at(contract, U256::ZERO)
        .await?;
    assert_eq!(public_value, U256::ZERO, "Private slot leaked to public");

    // Verify: contract can read its own storage (via eth_call)
    let internal_value = call_get_storage(&harness, contract).await?;
    assert_eq!(internal_value, U256::from(42), "Contract can't read own private storage");

    Ok(())
}
```

**Exit Criteria:**
- [ ] PrivacyAwareDatabase compiles and implements Database trait
- [ ] Reads route correctly based on classification
- [ ] Writes to private slots don't appear in public state
- [ ] Integration test with TestHarness passes

---

### Phase 3 Checkpoint: RPC Filtering

**Validation Tests:**

```bash
# RPC-specific tests
cargo test -p base-reth-rpc --test privacy_rpc
```

**Manual RPC Testing:**

```bash
# Start local node with privacy enabled
./target/release/base-reth-node --chain base-sepolia --http

# Deploy private contract (use forge or cast)
# Then query:

# Should return 0x0 (filtered)
cast storage <CONTRACT> 0x0 --rpc-url http://localhost:8545

# Should return real value (contract function)
cast call <CONTRACT> "getValue()" --rpc-url http://localhost:8545
```

**Exit Criteria:**
- [ ] `eth_getStorageAt` returns zero for private slots
- [ ] `eth_call` executes correctly (contracts read own private state)
- [ ] `eth_getLogs` filters events from privacy-configured contracts
- [ ] Non-private contracts work exactly as before

---

### Phase 4 Checkpoint: Block Publishing

**Validation Tests:**

```rust
#[tokio::test]
async fn test_state_diff_excludes_private() -> eyre::Result<()> {
    let harness = TestHarness::new().await?;

    // Setup: deploy + register private contract
    let contract = deploy_private_erc20(&harness).await?;

    // Do a transfer (modifies private balances slot)
    transfer_tokens(&harness, contract, alice, bob, 1000).await?;

    // Build the block
    let block = harness.get_latest_block().await?;

    // Verify state diff doesn't contain private slots
    let state_diff = get_block_state_diff(&harness, block.number).await?;

    for (addr, storage_changes) in state_diff.storage {
        if addr == contract {
            for (slot, _value) in storage_changes {
                let classification = classify_slot(&registry, contract, slot);
                assert!(
                    matches!(classification, SlotClassification::Public),
                    "Private slot {:?} leaked in state diff", slot
                );
            }
        }
    }

    Ok(())
}
```

**Exit Criteria:**
- [ ] State diffs exclude private storage changes
- [ ] Transaction receipts filter private events (if configured)
- [ ] Block can still be validated (state root correct for public state)

---

### Phase 5 Checkpoint: Persistence & Authorization

**Precompile Tests:**

```bash
# Test precompile registration and execution
cargo test -p base-reth-privacy --test precompile_integration
```

**Manual Precompile Testing:**

```bash
# Call privacy registry precompile
cast call 0x0000000000000000000000000000000000000200 \
    "isRegistered(address)" <CONTRACT> \
    --rpc-url http://localhost:8545

# Register via precompile (from deployer account)
cast send 0x0000000000000000000000000000000000000200 \
    "register(address,(uint256,uint8,uint8)[])" \
    <CONTRACT> "[(0,1,1)]" \
    --private-key <DEPLOYER_KEY> \
    --rpc-url http://localhost:8545
```

**Exit Criteria:**
- [ ] Precompile at 0x200 responds to calls
- [ ] Precompile at 0x201 handles authorization
- [ ] State persists across node restarts (if persistence implemented)
- [ ] Gas costs are reasonable (<50k for registration)

---

### Phase 6 Checkpoint: Solidity Library

**Forge Tests:**

```bash
cd packages/privacy
forge test -vvv
```

**Integration with Local Node:**

```bash
# Deploy PrivateERC20 to local node
forge create src/tokens/PrivateERC20.sol:PrivateERC20 \
    --constructor-args "Test" "TST" 1000000000000000000000000 \
    --rpc-url http://localhost:8545 \
    --private-key $DEPLOYER_KEY

# Verify auto-registration
cast call 0x0000000000000000000000000000000000000200 \
    "isRegistered(address)" <DEPLOYED_ADDRESS>
# Should return: true

# Transfer tokens
cast send <TOKEN> "transfer(address,uint256)" $BOB 1000000000000000000 \
    --private-key $ALICE_KEY \
    --rpc-url http://localhost:8545

# Verify privacy
cast storage <TOKEN> $(cast keccak "$(cast abi-encode 'f(address,uint256)' $BOB 0)")
# Should return: 0x0
```

**Exit Criteria:**
- [ ] All Forge tests pass
- [ ] PrivateERC20 deploys and auto-registers
- [ ] Transfers work correctly
- [ ] Balance slots are private
- [ ] totalSupply slot is public

---

### Full System Validation

**E2E Test Suite:**

```bash
# Run complete E2E test suite
cargo nextest run -p e2e-tests --test privacy

# Manual smoke test
./scripts/privacy-smoke-test.sh
```

**Smoke Test Script:**

```bash
#!/bin/bash
# scripts/privacy-smoke-test.sh

set -e

echo "=== Privacy Layer Smoke Test ==="

# 1. Start node
./target/release/base-reth-node --chain base-sepolia --http &
NODE_PID=$!
sleep 5

# 2. Deploy private token
TOKEN=$(forge create packages/privacy/src/tokens/PrivateERC20.sol:PrivateERC20 \
    --constructor-args "SmokeTest" "SMK" 1000000000000000000000000 \
    --rpc-url http://localhost:8545 \
    --private-key $DEPLOYER_KEY \
    --json | jq -r '.deployedTo')

echo "Deployed token: $TOKEN"

# 3. Check registration
REGISTERED=$(cast call 0x200 "isRegistered(address)" $TOKEN --rpc-url http://localhost:8545)
if [ "$REGISTERED" != "true" ]; then
    echo "FAIL: Token not registered"
    kill $NODE_PID
    exit 1
fi
echo "PASS: Token registered"

# 4. Transfer tokens
cast send $TOKEN "transfer(address,uint256)" $BOB 1000000000000000000 \
    --private-key $ALICE_KEY \
    --rpc-url http://localhost:8545

# 5. Check privacy
SLOT=$(cast keccak "$(cast abi-encode 'f(address,uint256)' $BOB 0)")
STORAGE=$(cast storage $TOKEN $SLOT --rpc-url http://localhost:8545)

if [ "$STORAGE" != "0x0000000000000000000000000000000000000000000000000000000000000000" ]; then
    echo "FAIL: Private slot leaked"
    kill $NODE_PID
    exit 1
fi
echo "PASS: Private slot returns zero"

# 6. Check balanceOf works
BALANCE=$(cast call $TOKEN "balanceOf(address)" $BOB --rpc-url http://localhost:8545)
if [ "$BALANCE" == "0x0" ]; then
    echo "FAIL: balanceOf returns zero"
    kill $NODE_PID
    exit 1
fi
echo "PASS: balanceOf returns correct value"

# Cleanup
kill $NODE_PID

echo "=== All smoke tests passed ==="
```

---

**End of Implementation Plan**

*Proceed to PRIVACY-TESTING-PLAN.md for comprehensive testing strategy.*
