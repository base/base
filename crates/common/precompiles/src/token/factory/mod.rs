//! `TokenFactory` native precompile — creates B-20 tokens at deterministic prefix-encoded addresses.

mod dispatch;
mod precompile;
mod storage;

pub use precompile::TokenFactoryPrecompile;
pub use storage::{
    B20_PREFIX_BYTE, B20_PREFIX_MARKER, CREATE_TOKEN_VERSION, FACTORY_ADDRESS, RESERVED_SIZE,
    TokenFactory, VARIANT_DEFAULT, VARIANT_NONE, address_prefix, compute_b20_address, decimals_of,
    has_b20_prefix, is_supported_variant, variant_of,
};
