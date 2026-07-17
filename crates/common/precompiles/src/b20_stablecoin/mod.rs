//! Stablecoin B-20 native precompile — stablecoin variant of the B-20 token.

mod abi;
pub use abi::IB20Stablecoin;

mod accounting;
pub use accounting::StablecoinAccounting;

mod context;
pub use context::ContractContext;

mod dispatch;

mod versions;
pub use versions::{Version, VersionResolver};

mod logic;
pub use logic::{B20StablecoinLogic, B20StablecoinLogicV1};

mod precompile;
pub use precompile::B20StablecoinPrecompile;

mod storage;
pub use storage::{B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinStorage};
