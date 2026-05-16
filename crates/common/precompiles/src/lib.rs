#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod provider;
pub use provider::BasePrecompiles;

mod installer;
pub use installer::BasePrecompileInstaller;

mod spec;
pub use spec::BasePrecompileSpec;

mod bn254_pair;

mod bls12_381;

pub mod token;
pub use token::{
    CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, DEFAULT_TOKEN_ADDRESS, DefaultToken,
    DefaultTokenStorage, IDefaultToken, Token, TokenAccounting,
    Burnable, Mintable, Pausable, Permittable, Redeemable, Configurable, Transferable,
};
