#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod macros;

mod provider;
pub use provider::BasePrecompiles;

mod spec;
pub use spec::BasePrecompileSpec;

mod activation;
pub use activation::{ActivationRegistry, ActivationRegistryPrecompile, IActivationRegistry};

mod bn254_pair;

mod bls12_381;

mod common;
pub use common::{
    Burnable, CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, Configurable, Mintable, Pausable,
    Permittable, Policy, Redeemable, Token, TokenAccounting, Transferable,
};

mod b20;
pub use b20::{B20Token, B20TokenPrecompile, B20TokenStorage, IB20};

mod factory;
pub use factory::{ITokenFactory, TokenFactory, TokenFactoryPrecompile, TokenVariant};

mod policy_registry;
pub use policy_registry::{
    IPolicyRegistry, POLICY_REGISTRY_ADDRESS, PolicyHandle, PolicyRegistryEvm,
};
