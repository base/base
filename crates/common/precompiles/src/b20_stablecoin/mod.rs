//! Stablecoin variant of the B-20 token, structured as a hardfork-versioned
//! precompile: a dispatcher, an append-only ABI, a version manager, shared
//! storage, and per-version self-contained logic implementations.

mod abi;
pub use abi::IB20Stablecoin;

mod accounting;
pub use accounting::StablecoinAccounting;

mod dispatch;

mod versions;
pub use versions::{StablecoinVersion, StablecoinVersions};

mod logic;
pub use logic::{B20StablecoinToken, StablecoinLogic, StablecoinV1};

mod precompile;
pub use precompile::B20StablecoinPrecompile;

mod storage;
pub use storage::{B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinStorage};
