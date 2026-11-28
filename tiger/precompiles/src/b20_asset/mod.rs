//! Asset B-20 native precompile — asset variant of the B-20 token.

mod abi;
pub use abi::{ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20Asset};

mod accounting;
pub use accounting::AssetAccounting;

mod dispatch;

mod logic;

mod storage;
pub use storage::{B20AssetExtensionStorage, B20AssetInit, B20AssetStorage};
