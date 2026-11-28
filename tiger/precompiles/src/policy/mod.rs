//! `PolicyRegistry` native precompile — global singleton transfer-policy registry for B-20 tokens.

mod abi;
pub use abi::{IPolicyRegistry, IPolicyRegistryV1};

mod accounting;
pub use accounting::PolicyAccounting;

mod dispatch;

mod versions;
pub use versions::{PolicyVersion, PolicyVersions};

mod logic;
pub use logic::PolicyRegistry;

mod storage;
pub use storage::{PackedPolicy, PolicyRegistryStorage};
