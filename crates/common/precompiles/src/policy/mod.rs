//! `PolicyRegistry` native precompile — global singleton transfer-policy registry for B-20 tokens.

mod abi;
pub use abi::IPolicyRegistry;

mod dispatch;

mod precompile;
pub use precompile::PolicyRegistryPrecompile;

mod handle;
pub use handle::PolicyHandle;

mod storage;
pub use storage::{PackedPolicy, PolicyRegistryStorage};
