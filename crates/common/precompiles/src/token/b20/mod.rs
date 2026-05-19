//! `B20Token` native precompile — the core B-20 token implementation.

mod dispatch;
mod precompile;
mod storage;
mod token;

pub use precompile::B20TokenPrecompile;
pub use storage::{B20_TOKEN_ADDRESS, B20TokenStorage};
pub use token::B20Token;
