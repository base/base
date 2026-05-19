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

mod token;
pub use token::{
    Burnable, CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, Configurable, DEFAULT_PREFIX,
    DEFAULT_TOKEN_ADDRESS, DefaultToken, DefaultTokenEvm, DefaultTokenStorage, FACTORY_ADDRESS,
    IDefaultToken, ITokenFactory, Mintable, Pausable, Permittable, RESERVED_SIZE, Redeemable,
    SECURITY_PREFIX, STABLECOIN_PREFIX, Token, TokenAccounting, TokenFactory, TokenFactoryEvm,
    Transferable, VARIANT_DEFAULT, VARIANT_NONE, VARIANT_SECURITY, VARIANT_STABLECOIN,
    compute_default_address, compute_security_address, compute_stablecoin_address, has_b20_prefix,
    variant_of,
};
