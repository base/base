//! Privacy Precompiles
//!
//! This module implements two precompiles for the privacy layer:
//!
//! - **0x0200 (Registry)**: Allows contracts to register for privacy protection
//! - **0x0201 (Auth)**: Allows slot owners to grant/revoke read access
//!
//! # Architecture
//!
//! Since revm's `PrecompileFn` is stateless (`fn(&[u8], u64) -> PrecompileResult`),
//! we use thread-local storage to provide access to the privacy registry and store
//! during EVM execution. The flow is:
//!
//! 1. Before transaction execution, call `set_context()` with the registry/store
//! 2. During execution, precompile calls access TLS via `with_context()`
//! 3. After execution, call `clear_context()` to clean up
//!
//! # EVM Integration
//!
//! To use these precompiles with reth/revm, you need to:
//!
//! 1. Get the precompiles using [`privacy_precompiles()`]
//! 2. Add them to your EVM's precompile set using `Precompiles::extend()`
//! 3. Set up the context before each transaction
//!
//! ```ignore
//! use base_reth_privacy::precompiles::{privacy_precompiles, setup_precompile_context, clear_context};
//! use revm::precompile::Precompiles;
//!
//! // Create your base precompiles (e.g., from OpPrecompiles)
//! let mut precompiles = Precompiles::cancun().clone();
//!
//! // Add privacy precompiles
//! precompiles.extend(privacy_precompiles());
//!
//! // Before executing each transaction:
//! setup_precompile_context(registry, store, sender, block_number);
//!
//! // Execute transaction...
//!
//! // After transaction:
//! clear_context();
//! ```
//!
//! # ABI Encoding
//!
//! We use Solidity-compatible ABI encoding with function selectors for easy
//! integration with smart contracts. See [`encoding`] module for details.
//!
//! # Security
//!
//! - Registry: Only the contract itself (msg.sender) can register
//! - Auth: Only the slot owner can grant/revoke access
//! - All inputs are validated before processing

mod constants;
mod context;
mod encoding;
mod error;

pub mod auth;
pub mod registry;

pub use constants::{AUTH_PRECOMPILE_ADDRESS, REGISTRY_PRECOMPILE_ADDRESS};
pub use context::{clear_context, set_context, PrecompileContext};
pub use encoding::{
    AuthGrantInput, AuthRevokeInput, IsAuthorizedInput, RegistrationInput, SlotConfigInput,
};
pub use error::PrecompileInputError;

use crate::{PrivacyRegistry, PrivateStateStore};
use alloy_primitives::Address;
use revm::precompile::{Precompile, PrecompileId};
use std::sync::Arc;

/// Gas cost for registry operations (base cost)
pub const REGISTRY_BASE_GAS: u64 = 5000;
/// Gas cost per slot registered
pub const REGISTRY_PER_SLOT_GAS: u64 = 2000;
/// Gas cost for auth operations
pub const AUTH_BASE_GAS: u64 = 3000;

/// Creates the privacy precompiles for registration with the EVM.
///
/// # Returns
///
/// A vector containing the Registry (0x200) and Auth (0x201) precompiles.
///
/// # Example
///
/// ```ignore
/// use base_reth_privacy::precompiles::privacy_precompiles;
///
/// let precompiles = privacy_precompiles();
/// // Add to your Precompiles set
/// my_precompiles.extend(precompiles);
/// ```
pub fn privacy_precompiles() -> Vec<Precompile> {
    vec![
        Precompile::new(
            PrecompileId::Custom("PrivacyRegistry".into()),
            REGISTRY_PRECOMPILE_ADDRESS,
            registry::run,
        ),
        Precompile::new(
            PrecompileId::Custom("PrivacyAuth".into()),
            AUTH_PRECOMPILE_ADDRESS,
            auth::run,
        ),
    ]
}

/// Helper function to set up the precompile context for execution.
///
/// This must be called before executing any transaction that might call
/// the privacy precompiles.
///
/// # Arguments
///
/// * `registry` - The privacy registry
/// * `store` - The private state store
/// * `caller` - The address making the call (msg.sender in precompile context)
/// * `block` - The current block number
///
/// # Example
///
/// ```ignore
/// use base_reth_privacy::precompiles::setup_precompile_context;
///
/// setup_precompile_context(registry, store, caller, block_number);
/// // ... execute transaction ...
/// clear_context();
/// ```
pub fn setup_precompile_context(
    registry: Arc<PrivacyRegistry>,
    store: Arc<PrivateStateStore>,
    caller: Address,
    block: u64,
) {
    set_context(PrecompileContext::new(registry, store, caller, block));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_privacy_precompiles_addresses() {
        let precompiles = privacy_precompiles();
        assert_eq!(precompiles.len(), 2);

        let registry = &precompiles[0];
        let auth = &precompiles[1];

        assert_eq!(*registry.address(), REGISTRY_PRECOMPILE_ADDRESS);
        assert_eq!(*auth.address(), AUTH_PRECOMPILE_ADDRESS);
    }

    #[test]
    fn test_precompile_addresses_are_distinct() {
        assert_ne!(REGISTRY_PRECOMPILE_ADDRESS, AUTH_PRECOMPILE_ADDRESS);
    }
}
