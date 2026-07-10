//! `B20AssetToken` native precompile — asset variant of the B-20 token.

mod abi;
pub use abi::IB20Asset;

mod accounting;
pub use accounting::AssetAccounting;

mod logic;
pub use logic::{AssetLogic, AssetLogicId, AssetLogicV1, AssetLogicV2, AssetOps, asset_logic_for};

mod dispatch;

mod precompile;
pub use precompile::B20AssetPrecompile;

mod storage;
pub use storage::{B20AssetExtensionStorage, B20AssetInit, B20AssetStorage};

mod token;
pub use token::B20AssetToken;
