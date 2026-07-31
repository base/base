#![allow(dead_code, non_snake_case)]

use base_precompile_macros::Storable;

#[derive(Storable)]
struct DuplicateFields {
    foo: u8,
    FOO: u8,
}

fn main() {}
