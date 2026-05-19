//! `TokenFactory` native precompile — creates B-20 tokens at deterministic prefix-encoded addresses.

mod dispatch;
mod evm;
mod storage;

pub use evm::TokenFactoryEvm;
pub use storage::{
    DEFAULT_PREFIX, FACTORY_ADDRESS, RESERVED_SIZE, SECURITY_PREFIX, STABLECOIN_PREFIX,
    TokenFactory, VARIANT_DEFAULT, VARIANT_NONE, VARIANT_SECURITY, VARIANT_STABLECOIN,
    compute_default_address, compute_security_address, compute_stablecoin_address, has_b20_prefix,
    variant_of,
};
