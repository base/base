//! Combining `#[namespace]` and `#[base_slot]` on the same field must be a compile error.
use alloy_primitives::U256;
use base_precompile_macros::contract;

#[contract]
struct Foo {
    #[base_slot(0)]
    #[namespace("ns.field")]
    value: U256,
}

fn main() {}
