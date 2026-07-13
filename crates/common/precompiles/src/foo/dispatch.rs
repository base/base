//! ABI dispatch for the `foo` reference precompile.
//!
//! The dispatcher receives the active [`FooVersion`] and resolves it to a single
//! implementation once, via [`FooVersion::logic`]. It then decodes calldata and
//! routes each selector to that implementation — there is no per-selector match
//! on the version, so adding a version touches only [`FooVersion::logic`].

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    FooStorage, FooVersion,
    IFoo::{self, IFooCalls as C},
    macros::decode_precompile_call,
};

/// Per-word calldata gas charge (`G_SHA3WORD`), matching other Base precompiles.
const CALLDATA_WORD_GAS: u64 = 6;

impl FooStorage<'_> {
    /// ABI-dispatches `foo` calldata against the `version` active for this block.
    pub fn dispatch(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: FooVersion,
    ) -> PrecompileResult {
        let calldata_cost =
            (calldata.len() as u64).div_ceil(32).saturating_mul(CALLDATA_WORD_GAS);
        if let Err(error) = ctx.deduct_gas(calldata_cost) {
            return error.into_precompile_result(ctx.gas_used(), ctx.state_gas_used());
        }
        self.inner(ctx, calldata, version).into_precompile_result(
            ctx.gas_used(),
            ctx.state_gas_used(),
            0,
            |output| output,
        )
    }

    fn inner(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: FooVersion,
    ) -> base_precompile_storage::Result<Bytes> {
        // Resolve the version to its implementation once; route ABI calls to it.
        let logic = version.logic();
        match decode_precompile_call!(calldata, IFoo::IFooCalls) {
            C::helloWorld(_) => {
                Ok(IFoo::helloWorldCall::abi_encode_returns(&logic.hello_world()).into())
            }
            C::greet(call) => {
                let greeting = logic.greet(self, ctx.caller(), call.name)?;
                Ok(IFoo::greetCall::abi_encode_returns(&greeting).into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use alloy_sol_types::SolCall;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::{FooStorage, FooVersion, IFoo};

    const CALLER: Address = address!("0x1111111111111111111111111111111111111111");

    fn dispatch(
        storage: &mut HashMapStorageProvider,
        calldata: &[u8],
        version: FooVersion,
    ) -> revm::precompile::PrecompileOutput {
        StorageCtx::enter(storage, |ctx| FooStorage::new(ctx).dispatch(ctx, calldata, version))
            .expect("dispatch should not fail fatally")
    }

    #[test]
    fn hello_world_differs_across_versions() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata = IFoo::helloWorldCall {}.abi_encode();

        let v1 = dispatch(&mut storage, &calldata, FooVersion::V1);
        assert!(!v1.is_revert());
        assert_eq!(IFoo::helloWorldCall::abi_decode_returns(&v1.bytes).unwrap(), "Hello, World!");

        let v2 = dispatch(&mut storage, &calldata, FooVersion::V2);
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

        let output = dispatch(&mut storage, &calldata, FooVersion::V1);

        assert!(output.is_revert(), "greet must revert before its activation version");
    }

    #[test]
    fn greet_returns_personalized_greeting_from_v2() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(CALLER);
        let calldata = IFoo::greetCall { name: "base".into() }.abi_encode();

        let output = dispatch(&mut storage, &calldata, FooVersion::V2);

        assert!(!output.is_revert());
        assert_eq!(IFoo::greetCall::abi_decode_returns(&output.bytes).unwrap(), "Hello, base!");
    }

    #[test]
    fn greet_greeting_changes_in_v3() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(CALLER);
        let calldata = IFoo::greetCall { name: "base".into() }.abi_encode();

        let output = dispatch(&mut storage, &calldata, FooVersion::V3);

        assert!(!output.is_revert());
        assert_eq!(
            IFoo::greetCall::abi_decode_returns(&output.bytes).unwrap(),
            "Hey base, welcome to Base!"
        );
    }

    #[test]
    fn hello_world_is_unchanged_from_v2_in_v3() {
        let mut storage = HashMapStorageProvider::new(1);
        let calldata = IFoo::helloWorldCall {}.abi_encode();

        let v2 = dispatch(&mut storage, &calldata, FooVersion::V2);
        let v3 = dispatch(&mut storage, &calldata, FooVersion::V3);

        assert_eq!(
            IFoo::helloWorldCall::abi_decode_returns(&v2.bytes).unwrap(),
            IFoo::helloWorldCall::abi_decode_returns(&v3.bytes).unwrap(),
        );
    }

    #[test]
    fn dispatch_reverts_on_unknown_selector() {
        let mut storage = HashMapStorageProvider::new(1);
        let output = dispatch(&mut storage, &[0xde, 0xad, 0xbe, 0xef], FooVersion::V2);

        assert!(output.is_revert());
    }
}
