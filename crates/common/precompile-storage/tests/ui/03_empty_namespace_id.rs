//! An empty string passed to `#[namespace]` must be a compile error.

use base_precompile_macros::contract;

#[contract]
struct Foo {
    #[namespace("")]
    value: u64,
}

fn main() {}
