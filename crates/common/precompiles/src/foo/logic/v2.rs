//! Version 2 of the `foo` precompile, activated at Cobalt.
//!
//! Self-contained copy of V1's shape with the changed/added behavior written
//! out in full: `helloWorld` returns a new string and `greet` is now routed and
//! implemented. V1 is untouched and stays frozen.

use alloc::{format, string::ToString};

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{Result, StorageCtx};

use crate::{
    FooStorage, FooVersion,
    IFoo::{self, IFooCalls as C},
    macros::decode_precompile_call,
};

/// Second `foo` implementation, activated at Cobalt.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooV2;

impl FooVersion for FooV2 {
    fn call(
        &self,
        storage: &mut FooStorage<'_>,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
    ) -> Result<Bytes> {
        match decode_precompile_call!(calldata, IFoo::IFooCalls) {
            C::helloWorld(_) => {
                // Goal 1: changed behavior. V1 returned "Hello, World!".
                Ok(IFoo::helloWorldCall::abi_encode_returns(
                    &"Hello, World! Welcome to Base.".to_string(),
                )
                .into())
            }
            C::greet(call) => {
                // Goal 3: new method, routed and implemented from Cobalt onward.
                let greeting = format!("Hello, {}!", call.name);
                storage.record_greeting(ctx.caller(), &greeting)?;
                Ok(IFoo::greetCall::abi_encode_returns(&greeting).into())
            }
        }
    }
}
