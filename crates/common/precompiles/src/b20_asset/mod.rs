//! Asset B-20 native precompile — asset variant of the B-20 token.

mod abi;
pub use abi::IB20Asset;

mod accounting;
pub use accounting::AssetAccounting;

mod context;
pub use context::ContractContext;

mod dispatch;

mod versions;
pub use versions::{Version, VersionResolver};

mod logic;
pub use logic::{Logic, LogicV1};

mod precompile;
pub use precompile::B20AssetPrecompile;

mod storage;
pub use storage::{B20AssetExtensionStorage, B20AssetInit, B20AssetStorage};
