//! `PolicyRegistry` native precompile — global singleton transfer-policy registry for B-20 tokens.

mod abi;
pub use abi::IPolicyRegistry;

mod accounting;
pub use accounting::PolicyAccounting;

mod dispatch;

mod versions;
pub use versions::{PolicyVersion, PolicyVersions};

mod logic;
pub use logic::{PolicyRegistryLogic, PolicyRegistryV1};

mod precompile;
pub use precompile::PolicyRegistryPrecompile;

mod storage;
pub use storage::{PackedPolicy, PolicyRegistryStorage};
