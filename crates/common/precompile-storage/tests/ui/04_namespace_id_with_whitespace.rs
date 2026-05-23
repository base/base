//! A namespace id containing whitespace must be a compile error.

use base_precompile_macros::contract;

#[contract]
struct Foo {
    #[namespace("foo bar")]
    value: u64,
}

fn main() {}
