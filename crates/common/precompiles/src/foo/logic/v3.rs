//! Version 3 of the `foo` precompile logic, staged for the next hardfork.

use alloc::{format, string::String};

use alloy_primitives::Address;
use base_precompile_storage::Result;

use crate::{FooLogic, FooStorage, FooV2};

/// Third `foo` implementation.
///
/// Shows both halves of the composition pattern in one version: `hello_world`
/// is unchanged since V2, so it delegates to `self.previous` rather than copying
/// the string; `greet` changes, so it is overridden here. V1 and V2 stay frozen.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooV3 {
    /// The immutable predecessor this version builds on.
    pub previous: FooV2,
}

impl FooLogic for FooV3 {
    fn hello_world(&self) -> String {
        // Unchanged since V2: reuse the frozen implementation instead of
        // duplicating it.
        self.previous.hello_world()
    }

    fn greet(&self, storage: &mut FooStorage<'_>, caller: Address, name: String) -> Result<String> {
        // Goal 1: changed behavior. V2 returned "Hello, {name}!".
        let greeting = format!("Hey {name}, welcome to Base!");
        storage.record_greeting(caller, &greeting)?;
        Ok(greeting)
    }
}
