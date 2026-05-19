//! Business logic for the `PolicyRegistry` precompile.
//!
//! Each method here corresponds to one or more `IPolicyRegistry` ABI calls.
//! `dispatch.rs` decodes calldata and delegates here; no ABI encoding lives in
//! this file.

use super::storage::PolicyRegistryStorage;

impl PolicyRegistryStorage<'_> {
    /// Placeholder — returns `true` unconditionally.
    pub fn hello_world(&self) -> bool {
        true
    }
}
