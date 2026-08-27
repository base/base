//! `B20AssetToken` native precompile — asset variant of the B-20 token.

mod abi;
pub use abi::{ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20Asset, IB20AssetV1, IB20AssetV2};

mod accounting;
pub use accounting::AssetAccounting;

mod dispatch;

mod versions;
pub(crate) use versions::AssetCall;
pub use versions::{AssetAbi, AssetAbiPair, AssetVersion, AssetVersions};

mod logic;
pub use logic::{Asset, AssetV1, AssetV2, B20AssetToken};

mod precompile;
pub use precompile::B20AssetPrecompile;

mod storage;
pub use storage::{B20AssetExtensionStorage, B20AssetInit, B20AssetStorage};
