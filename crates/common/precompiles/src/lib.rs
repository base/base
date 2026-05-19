#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod macros;

mod provider;
pub use provider::BasePrecompiles;

mod installer;
pub use installer::BasePrecompileInstaller;

mod spec;
pub use spec::BasePrecompileSpec;

mod bn254_pair;

mod bls12_381;

mod token;
pub use token::{
    B20_TOKEN_ADDRESS, B20Token, B20TokenPrecompile, B20TokenStorage, Burnable,
    CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, Configurable, IB20, ITokenFactory, Mintable,
    Pausable, Permittable, Redeemable, Token, TokenAccounting, TokenFactory,
    TokenFactoryPrecompile, TokenVariant, Transferable,
};
