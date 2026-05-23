//! Field-level `#[slot]` cannot be used when a contract-level `#[namespace]` is active.
use alloy_primitives::U256;
use base_precompile_macros::{contract, namespace};

#[namespace("ns.contract")]
#[contract]
struct Foo {
    #[slot(0)]
    value: U256,
}

fn main() {}
