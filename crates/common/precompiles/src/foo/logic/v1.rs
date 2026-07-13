//! Version 1 of the `foo` precompile, activated at Beryl.
//!
//! Self-contained: V1 owns its own routing and logic. Selectors it does not
//! implement (e.g. `greet`, added in V2) fall through to the unsupported
//! revert, so V1 needs no edits when later versions add methods — it stays
//! frozen.

use alloc::string::ToString;

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};

use crate::{
    FooStorage, FooVersion,
    IFoo::{self, IFooCalls as C},
    macros::decode_precompile_call,
};

/// First `foo` implementation. Frozen as of its activation at Beryl.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooV1;

impl FooVersion for FooV1 {
    fn call(
        &self,
        _storage: &mut FooStorage<'_>,
        _ctx: StorageCtx<'_>,
        calldata: &[u8],
    ) -> Result<Bytes> {
        match decode_precompile_call!(calldata, IFoo::IFooCalls) {
            C::helloWorld(_) => {
                Ok(IFoo::helloWorldCall::abi_encode_returns(&"Hello, World!".to_string()).into())
            }
            // Any other (valid) selector had not been introduced at V1; reverting
            // as unsupported preserves the original pre-activation behavior.
            _ => Err(BasePrecompileError::revert(IFoo::UnsupportedBeforeActivation {})),
        }
    }
}
