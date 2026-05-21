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
pub use activation::{ActivationRegistry, ActivationRegistryStorage, IActivationRegistry};

mod bn254_pair;

mod bls12_381;

mod common;
pub use common::{
    B20Guards, B20TokenRole, Burnable, Configurable, Mintable, Pausable, Permittable, Policy,
    PolicyRegistry, RoleManaged, Token, TokenAccounting, Transferable,
};
#[cfg(any(test, feature = "test-utils"))]
pub use common::{InMemoryPolicy, InMemoryTokenAccounting, TestToken};

mod b20;
pub use b20::{
    B20PausableFeature, B20PolicyType, B20Token, B20TokenPrecompile, B20TokenStorage, IB20,
    POLICY_ALWAYS_ALLOW, POLICY_ALWAYS_BLOCK,
};

mod factory;
pub use factory::{ITokenFactory, TokenFactory, TokenFactoryStorage, TokenVariant};

mod policy;
pub use policy::{IPolicyRegistry, PolicyHandle, PolicyRegistryPrecompile, PolicyRegistryStorage};
