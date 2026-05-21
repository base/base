//! `B20StablecoinToken` native precompile — stablecoin variant of the B-20 token.

mod abi;
pub use abi::IB20Stablecoin;

mod accounting;
pub use accounting::StablecoinAccounting;

mod dispatch;

mod precompile;
pub use precompile::B20StablecoinPrecompile;

mod storage;
pub use storage::B20StablecoinStorage;

mod token;
pub use token::B20StablecoinToken;
