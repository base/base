//! `TokenFactory` native precompile — creates B-20 tokens at deterministic prefix-encoded addresses.

mod abi;
mod dispatch;
pub use abi::ITokenFactory;

mod precompile;
pub use precompile::TokenFactoryPrecompile;

mod storage;
pub use storage::TokenFactory;

mod variant;
pub use variant::TokenVariant;
