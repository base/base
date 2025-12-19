//! Privacy Layer for Base node-reth
//!
//! This crate implements a dual-store architecture for private contract storage,
//! allowing smart contract storage to be selectively hidden from public view
//! while remaining accessible to authorized parties.
//!
//! # Architecture
//!
//! The privacy layer works by routing SLOAD/SSTORE operations to different backends:
//! - **Public slots** → triedb (Base state trie)
//! - **Private slots** → TEE-local HashMap
//!
//! # Core Components
//!
//! - [`PrivacyRegistry`]: Manages contract registration and slot configurations
//! - [`PrivateStateStore`]: In-memory storage for private slot values
//! - [`PrivacyDatabase`]: Database wrapper that intercepts storage operations
//! - [`classify_slot`]: Determines if a slot is public or private
//!
//! # Example
//!
//! ```ignore
//! use base_reth_privacy::{PrivacyRegistry, PrivateStateStore, PrivacyDatabase};
//! use alloy_primitives::{Address, U256};
//! use std::sync::Arc;
//!
//! // Create registry and store
//! let registry = Arc::new(PrivacyRegistry::new());
//! let store = Arc::new(PrivateStateStore::new());
//!
//! // Wrap your database with privacy awareness
//! let privacy_db = PrivacyDatabase::new(inner_db, registry, store);
//!
//! // Use privacy_db with revm - private slots automatically routed
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod classification;
pub mod database;
pub mod evm;
pub mod inspector;
pub mod precompiles;
pub mod registry;
pub mod rpc;
pub mod store;

// Re-exports for convenient access
pub use classification::{classify_slot, SlotClassification};
pub use database::{PrivacyDatabase, PrivacyDatabaseError};
pub use registry::{
    OwnershipType, PrivacyRegistry, PrivateContractConfig, RegistryError, SlotConfig, SlotType,
};
pub use precompiles::{
    clear_context, privacy_precompiles, set_context, setup_precompile_context,
    PrecompileContext, AUTH_PRECOMPILE_ADDRESS, REGISTRY_PRECOMPILE_ADDRESS,
};
pub use rpc::{PrivacyRpcFilter, RpcFilterConfig, StorageFilterResult};
pub use store::{AuthEntry, PrivateEntry, PrivateStateStore, StoreError, READ, WRITE};
pub use evm::PrivacyEvmFactory;
pub use inspector::{PrivacyInspector, SlotKeyCache};
