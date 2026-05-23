//! Two `#[namespace]` attributes on the same field inside a `#[contract]` struct must be a
//! compile error.

use base_precompile_macros::contract;

#[contract]
struct Foo {
    #[namespace("ns.a")]
    #[namespace("ns.b")]
    value: u64,
}

fn main() {}
