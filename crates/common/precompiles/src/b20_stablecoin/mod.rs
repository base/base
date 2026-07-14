//! `B20StablecoinToken` native precompile — stablecoin variant of the B-20 token.

mod abi;
pub use abi::IB20Stablecoin;

mod accounting;
pub use accounting::StablecoinAccounting;

mod dispatch;

mod versions;
pub use versions::{StablecoinVersion, StablecoinVersions};

mod logic;
pub use logic::{B20StablecoinToken, Stablecoin, StablecoinV1};

mod precompile;
pub use precompile::B20StablecoinPrecompile;

mod storage;
pub use storage::{B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinStorage};
