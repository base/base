//! Field-level `#[slot]` cannot be used when a contract-level `#[namespace]` is active.

use base_precompile_macros::{contract, namespace};

#[namespace("ns.contract")]
#[contract]
struct Foo {
    #[slot(0)]
    value: u64,
}

fn main() {}
