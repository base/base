//! Shared precompile entry for the `foo` reference precompile.
//!
//! This is the *only* shared execution glue: it charges the flat calldata cost
//! and hands off to the active version's own [`FooVersion::call`]. Selector
//! decoding and routing live inside each version (see [`crate::logic`]), so a
//! version fully owns "which selectors exist and what they do".

use base_precompile_storage::{IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{FooStorage, FooVersion};

/// Per-word calldata gas charge (`G_SHA3WORD`), matching other Base precompiles.
const CALLDATA_WORD_GAS: u64 = 6;

impl FooStorage<'_> {
    /// Charges calldata gas, then dispatches to the `version` active for this
    /// block. The version decodes and routes the call itself.
    pub fn dispatch(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: &'static dyn FooVersion,
    ) -> PrecompileResult {
        let calldata_cost =
            (calldata.len() as u64).div_ceil(32).saturating_mul(CALLDATA_WORD_GAS);
        if let Err(error) = ctx.deduct_gas(calldata_cost) {
            return error.into_precompile_result(ctx.gas_used(), ctx.state_gas_used());
        }
        version.call(self, ctx, calldata).into_precompile_result(
            ctx.gas_used(),
            ctx.state_gas_used(),
            0,
            |output| output,
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use alloy_sol_types::SolCall;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{FooStorage, FooV1, FooV2, FooVersion, IFoo};

    const CALLER: Address = address!("0x1111111111111111111111111111111111111111");

    fn dispatch(
        storage: &mut HashMapStorageProvider,
        calldata: &[u8],
        version: &'static dyn FooVersion,
    ) -> revm::precompile::PrecompileOutput {
        StorageCtx::enter(storage, |ctx| FooStorage::new(ctx).dispatch(ctx, calldata, version))
            .expect("dispatch should not fail fatally")
    }

    #[test]
    fn hello_world_differs_across_versions() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata = IFoo::helloWorldCall {}.abi_encode();

        let v1 = dispatch(&mut storage, &calldata, &FooV1);
        assert!(!v1.is_revert());
        assert_eq!(IFoo::helloWorldCall::abi_decode_returns(&v1.bytes).unwrap(), "Hello, World!");

        let v2 = dispatch(&mut storage, &calldata, &FooV2);
        assert!(!v2.is_revert());
        assert_eq!(
            IFoo::helloWorldCall::abi_decode_returns(&v2.bytes).unwrap(),
            "Hello, World! Welcome to Base."
        );
    }

    #[test]
    fn greet_is_unsupported_before_v2() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(CALLER);
        let calldata = IFoo::greetCall { name: "base".into() }.abi_encode();

        // V1's routing has no `greet` arm; it falls through to unsupported.
        let output = dispatch(&mut storage, &calldata, &FooV1);

        assert!(output.is_revert(), "greet must revert before its activation version");
    }

    #[test]
    fn greet_returns_personalized_greeting_from_v2() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(CALLER);
        let calldata = IFoo::greetCall { name: "base".into() }.abi_encode();

        let output = dispatch(&mut storage, &calldata, &FooV2);

        assert!(!output.is_revert());
        assert_eq!(IFoo::greetCall::abi_decode_returns(&output.bytes).unwrap(), "Hello, base!");
    }

    #[test]
    fn dispatch_reverts_on_unknown_selector() {
        let mut storage = HashMapStorageProvider::new(1);
        let output = dispatch(&mut storage, &[0xde, 0xad, 0xbe, 0xef], &FooV2);

        assert!(output.is_revert());
    }
}
