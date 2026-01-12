//! Account Abstraction (EIP-4337) UserOperation Indexer
//!
//! This crate provides an Execution Extension (ExEx) that indexes UserOperation events
//! from EIP-4337 EntryPoint contracts. It supports:
//!
//! - EntryPoint v0.6 (0x5ff137d4b0fdcd49dca30c7cf57e578a026d2789)
//! - EntryPoint v0.7 (0x0000000071727de22e5e9d8baf0edac6f37da032)
//! - EntryPoint v0.8 (0x4337084d9e255ff0702461cf8895ce9e3b5ff108)
//!
//! ## Usage
//!
//! ```ignore
//! use base_reth_account_abstraction_indexer::{
//!     account_abstraction_indexer_exex,
//!     UserOperationStorage,
//! };
//!
//! // Create shared storage
//! let storage = Arc::new(UserOperationStorage::new());
//!
//! // Install the ExEx
//! builder.install_exex("aa-indexer", |ctx| {
//!     account_abstraction_indexer_exex(ctx, storage.clone())
//! });
//!
//! // Query indexed operations
//! if let Some(op) = storage.get(&user_op_hash) {
//!     println!("Found operation: {:?}", op);
//! }
//! ```

mod event;
mod exex;
mod storage;

pub use event::{
    is_entry_point, try_parse_user_operation_ref, IndexedUserOperationRef, UserOperationEvent,
    ENTRYPOINT_V06, ENTRYPOINT_V07, ENTRYPOINT_V08, ENTRYPOINT_V09,
};
pub use exex::account_abstraction_indexer_exex;
pub use storage::UserOperationStorage;

