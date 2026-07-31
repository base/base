#![allow(dead_code, non_snake_case)]

use base_precompile_macros::contract;

#[contract]
struct DuplicateFields {
    foo: u8,
    FOO: u8,
}

fn main() {}
