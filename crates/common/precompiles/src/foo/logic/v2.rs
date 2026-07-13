//! Version 2 of the `foo` precompile logic, activated at Cobalt.

use alloc::{
    format,
    string::{String, ToString},
};

use alloy_primitives::Address;
use base_precompile_storage::Result;

use crate::{FooLogic, FooStorage, FooV1};

/// Second `foo` implementation, activated at Cobalt.
///
/// Composes [`FooV1`] rather than inheriting from it: methods whose behavior is
/// unchanged would delegate to `self.previous`, while changed or new behavior is
/// overridden here. This keeps V1 frozen and the blast radius of a change small.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooV2 {
    /// The immutable predecessor this version builds on.
    pub previous: FooV1,
}

impl FooLogic for FooV2 {
    fn hello_world(&self) -> String {
        // Goal 1: changed behavior. V1 returned "Hello, World!".
        "Hello, World! Welcome to Base.".to_string()
    }

    fn greet(&self, storage: &mut FooStorage<'_>, caller: Address, name: String) -> Result<String> {
        // Goal 3: new method, reachable only from Cobalt onward.
        let greeting = format!("Hello, {name}!");
        storage.record_greeting(caller, &greeting)?;
        Ok(greeting)
    }
}
