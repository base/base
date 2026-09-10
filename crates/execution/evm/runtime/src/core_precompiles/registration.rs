//! Precompile registration scaffold.
//!
//! Adding a new native precompile requires:
//! 1. One file implementing [`NativePrecompile`].
//! 2. One registration line in the precompile registry.

use alloy_primitives::Address;

use crate::{
    CryptoPrecompileResult as PrecompileResult,
    core_precompiles::storage_provider::PrecompileStorageProvider,
};

/// Trait that every native precompile must implement.
///
/// # Example
///
/// ```ignore
/// use crate::core_precompiles::registration::NativePrecompile;
/// use base_execution_evm_macros::contract;
///
/// #[contract(addr = MY_PRECOMPILE_ADDRESS)]
/// pub struct MyPrecompile { ... }
///
/// impl NativePrecompile for MyPrecompile {
///     const ADDRESS: Address = MY_PRECOMPILE_ADDRESS;
///     fn execute(storage: &mut dyn PrecompileStorageProvider) -> PrecompileResult {
///         StorageCtx::enter(storage, |ctx| {
///             let pc = MyPrecompile::new(ctx);
///             // dispatch calldata ...
///         })
///     }
/// }
/// ```
pub trait NativePrecompile {
    /// The precompile's canonical contract address.
    const ADDRESS: Address;

    /// Executes the precompile with the given storage provider.
    fn execute(storage: &mut dyn PrecompileStorageProvider) -> PrecompileResult;
}
