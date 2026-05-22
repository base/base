//! `TokenFactory` native precompile — creates B-20 tokens at deterministic prefix-encoded addresses.

mod abi;
mod dispatch;
pub use abi::ITokenFactory;

mod precompile;
pub use precompile::TokenFactory;

mod storage;
pub use storage::TokenFactoryStorage;

mod variant;
pub use variant::TokenVariant;
