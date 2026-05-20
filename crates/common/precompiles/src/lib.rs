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
pub use activation::{
    ActivationFeature, ActivationRegistry, ActivationRegistryStorage, IActivationRegistry,
};

mod bn254_pair;
pub use bn254_pair::{JOVIAN, JOVIAN_MAX_INPUT_SIZE};

mod bls12_381;
pub use bls12_381::{
    JOVIAN_G1_MSM, JOVIAN_G1_MSM_MAX_INPUT_SIZE, JOVIAN_G2_MSM, JOVIAN_G2_MSM_MAX_INPUT_SIZE,
    JOVIAN_PAIRING, JOVIAN_PAIRING_MAX_INPUT_SIZE,
};

mod common;
pub use common::{
    B20Guards, B20TokenRole, Burnable, Configurable, Mintable, Pausable, Permittable, Policy,
    PolicyRegistry, RoleManaged, Token, TokenAccounting, Transferable,
};
#[cfg(any(test, feature = "test-utils"))]
pub use common::{InMemoryPolicy, InMemoryTokenAccounting, TestStablecoinToken, TestToken};

mod b20;
pub use b20::{
    B20CoreStorage, B20PausableFeature, B20PolicyType, B20Token, B20TokenInit, B20TokenPrecompile,
    B20TokenStorage, IB20,
};

mod b20_security;
pub use b20_security::{
    B20RedeemStorage, B20SecurityExtensionStorage, B20SecurityInit, B20SecurityPrecompile,
    B20SecurityStorage, B20SecurityToken, IB20Security, SecurityAccounting,
};

mod b20_stablecoin;
pub use b20_stablecoin::{
    B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinPrecompile,
    B20StablecoinStorage, B20StablecoinToken, IB20Stablecoin, StablecoinAccounting,
};

mod factory;
pub use factory::{ITokenFactory, TokenFactory, TokenFactoryStorage, TokenVariant};

mod policy;
pub use policy::{IPolicyRegistry, PolicyHandle, PolicyRegistryPrecompile, PolicyRegistryStorage};
