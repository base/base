#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod macros;

/// Gas cost for ABI-decoding calldata of the given byte length.
///
/// Charges `G_sha3word` (6 gas) per 32-byte word, rounded up — the same rate the EVM uses for
/// data-processing operations (keccak256). The EVM has no universal precompile input cost;
/// each precompile defines its own. Using `G_sha3word` is the natural choice because ABI decoding
/// is proportional data-processing work, and it prevents large calldata from being free to
/// process — a potential attack vector without this charge.
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
    Permittable, Policy, Redeemable, Token, TokenAccounting, Transferable,
};
#[cfg(any(test, feature = "test-utils"))]
pub use common::{InMemoryPolicy, InMemoryTokenAccounting, TestToken};

mod b20;
pub use b20::{B20Token, B20TokenPrecompile, B20TokenStorage, IB20};

mod factory;
pub use factory::{ITokenFactory, TokenFactory, TokenFactoryStorage, TokenVariant};

mod policy;
pub use policy::{IPolicyRegistry, PolicyHandle, PolicyRegistry, PolicyRegistryStorage};
