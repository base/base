//! An empty string passed to `#[namespace]` must be a compile error.
use alloy_primitives::U256;
use base_precompile_macros::contract;

#[contract]
struct Foo {
    #[namespace("")]
    value: U256,
}

fn main() {}
