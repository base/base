//! Version 3 of the `foo` precompile logic, staged for the next hardfork.

use alloc::{
    format,
    string::{String, ToString},
};

use alloy_primitives::Address;
use base_precompile_storage::Result;

use crate::{FooLogic, FooStorage};

/// Third `foo` implementation.
///
/// A self-contained copy: `hello_world` is unchanged since V2, so its value is
/// copied here verbatim rather than delegated to a predecessor; `greet`
/// changes, so it is rewritten. V1 and V2 stay frozen.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooV3;

impl FooLogic for FooV3 {
    fn hello_world(&self) -> String {
        // Unchanged since V2: the literal is copied forward (copy-fork), so V2
        // stays frozen and V3 has no runtime dependency on it.
        "Hello, World! Welcome to Base.".to_string()
    }

    fn greet(&self, storage: &mut FooStorage<'_>, caller: Address, name: String) -> Result<String> {
        // Goal 1: changed behavior. V2 returned "Hello, {name}!".
        let greeting = format!("Hey {name}, welcome to Base!");
        storage.record_greeting(caller, &greeting)?;
        Ok(greeting)
    }
}
