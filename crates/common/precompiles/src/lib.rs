#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod macros;

/// Returns the EIP-3860-style input cost for calldata of the given length.
///
/// Charges `G_sha3word` (6 gas) per 32-byte word of calldata. This mirrors the cost model used
/// by EIP-3860 for initcode and prevents callers from passing arbitrarily large calldata to
/// precompiles at near-zero cost — without this, an attacker could force expensive ABI decoding
/// with a single transaction.
pub const fn input_cost(calldata_len: usize) -> u64 {
    const G_SHA3WORD: u64 = 6;
    calldata_len.div_ceil(32).saturating_mul(G_SHA3WORD as usize) as u64
}

mod provider;
pub use provider::BasePrecompiles;

mod spec;
pub use spec::BasePrecompileSpec;

mod activation;
pub use activation::{ActivationRegistry, ActivationRegistryStorage, IActivationRegistry};

mod bn254_pair;

mod bls12_381;

mod common;
pub use common::{
    Burnable, CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, Configurable, Mintable, Pausable,
    Permittable, Policy, PolicyRegistry, Redeemable, Token, TokenAccounting, Transferable,
};
#[cfg(any(test, feature = "test-utils"))]
pub use common::{InMemoryPolicy, InMemoryTokenAccounting, TestToken};

mod b20;
pub use b20::{B20Token, B20TokenPrecompile, B20TokenStorage, IB20};

mod factory;
pub use factory::{ITokenFactory, TokenFactory, TokenFactoryStorage, TokenVariant};

mod policy;
pub use policy::{
    IPolicyRegistry,
    // PolicyType is re-exported directly for ergonomics — callers write `PolicyType::ALLOWLIST`
    // rather than `IPolicyRegistry::PolicyType::ALLOWLIST`.
    IPolicyRegistry::PolicyType,
    PolicyHandle,
    PolicyRegistryPrecompile,
    PolicyRegistryStorage,
};
