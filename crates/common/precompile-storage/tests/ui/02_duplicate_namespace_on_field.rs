//! Two `#[namespace]` attributes on the same field inside a `#[contract]` struct must be a
//! compile error.
use alloy_primitives::U256;
use base_precompile_macros::contract;

#[contract]
struct Foo {
    #[namespace("ns.a")]
    #[namespace("ns.b")]
    value: U256,
}

fn main() {}
