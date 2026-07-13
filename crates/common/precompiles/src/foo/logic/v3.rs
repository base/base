//! Version 3 of the `foo` precompile logic, staged for the next hardfork.

use alloc::{
    format,
    string::{String, ToString},
};

use alloy_primitives::Address;
use base_precompile_storage::Result;

use crate::{FooLogic, FooStorage};

/// Third `foo` implementation, written as a fully self-contained copy.
///
/// Rather than composing the previous version, it restates every method's
/// behavior directly. This maximizes isolation — V3 shares no code with V1/V2
/// and can never be affected by a change to them — at the cost of duplicating
/// unchanged logic. Contrast with the composition style, where a version holds a
/// `previous` and delegates unchanged methods to it.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooV3;

impl FooLogic for FooV3 {
    fn hello_world(&self) -> String {
        // Unchanged since V2, but copied verbatim rather than delegated.
        "Hello, World! Welcome to Base.".to_string()
    }

    fn greet(&self, storage: &mut FooStorage<'_>, caller: Address, name: String) -> Result<String> {
        // Goal 1: changed behavior. V2 returned "Hello, {name}!".
        let greeting = format!("Hey {name}, welcome to Base!");
        storage.record_greeting(caller, &greeting)?;
        Ok(greeting)
    }
}
