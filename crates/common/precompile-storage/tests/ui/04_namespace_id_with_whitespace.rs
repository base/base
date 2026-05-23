//! A namespace id containing whitespace must be a compile error.
use alloy_primitives::U256;
use base_precompile_macros::contract;

#[contract]
struct Foo {
    #[namespace("foo bar")]
    value: U256,
}

fn main() {}
