//! `#[namespace]` applied to a plain struct with neither `#[contract]` nor `#[derive(Storable)]`
//! must be a compile error.
use base_precompile_macros::namespace;

#[namespace("my.contract")]
struct Foo {
    value: u64,
}

fn main() {}
