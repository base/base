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

#[cfg(feature = "std")]
mod default_token;
#[cfg(feature = "std")]
pub use default_token::{DEFAULT_ADMIN_ROLE, DEFAULT_TOKEN_ADDRESS, DefaultToken, ISSUER_ROLE};

mod bn254_pair;

mod bls12_381;
