//! `B20Factory` native precompile — creates B-20 tokens at deterministic prefix-encoded addresses.

mod abi;
pub use abi::IB20Factory;

mod context;
pub use context::FactoryContractContext;

mod dispatch;

mod logic;
pub use logic::{B20FactoryLogic, B20FactoryLogicV1, CommonParams, TokenCreateParams};

mod precompile;
pub use precompile::B20Factory;

mod storage;
pub use storage::B20FactoryStorage;

mod variant;
pub use variant::B20Variant;

mod versions;
pub use versions::{Version, VersionResolver};
