//! Version 2 of the `foo` precompile logic, activated at Cobalt.

use alloc::{
    format,
    string::{String, ToString},
};

use alloy_primitives::Address;
use base_precompile_storage::Result;

use crate::{FooLogic, FooStorage};

/// Second `foo` implementation, activated at Cobalt.
///
/// A self-contained copy: it does not reference [`FooV1`](crate::FooV1).
/// Changed behavior (`hello_world`) and new behavior (`greet`) are written out
/// in full here, so this file alone describes V2. V1 stays frozen.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooV2;

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
