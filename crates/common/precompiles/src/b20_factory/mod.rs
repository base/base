//! `B20Factory` native precompile — creates B-20 tokens at deterministic prefix-encoded addresses.

mod abi;
pub use abi::IB20Factory;

mod dispatch;

mod precompile;
pub use precompile::B20Factory;

mod storage;
pub use storage::{B20FactoryStorage, CommonParams, TokenCreateParams};

mod variant;
pub use variant::B20Variant;
