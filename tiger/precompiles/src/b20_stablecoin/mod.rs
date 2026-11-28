//! Stablecoin B-20 native precompile — stablecoin variant of the B-20 token.

mod abi;
pub use abi::IB20Stablecoin;

mod accounting;
pub use accounting::StablecoinAccounting;

mod dispatch;

mod versions;

mod storage;
pub use storage::{B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinStorage};
