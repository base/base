//! `PolicyRegistry` native precompile — global singleton transfer-policy registry for B-20 tokens.

mod abi;
pub use abi::{IPolicyRegistry, IPolicyRegistryV1, IPolicyRegistryV2};

mod accounting;
pub use accounting::PolicyAccounting;

mod dispatch;

mod versions;
pub use versions::{PolicyAbi, PolicyVersion, PolicyVersions};

mod logic;
pub use logic::{PolicyRegistryLogic, PolicyRegistryV1, PolicyRegistryV2};

mod precompile;
pub use precompile::PolicyRegistryPrecompile;

mod storage;
pub use storage::{PackedPolicy, PolicyRegistryStorage};
