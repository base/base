//! Version 1 of the `foo` precompile logic, activated at Beryl.

use alloc::string::{String, ToString};

use crate::FooLogic;

/// First `foo` implementation. Frozen as of its activation at Beryl.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooV1;

impl FooLogic for FooV1 {
    fn hello_world(&self) -> String {
        "Hello, World!".to_string()
    }

    // `greet` did not exist in V1: it uses the default `FooLogic::greet`, which
    // reverts as unsupported. Do not add it here — V1 is frozen.
}
