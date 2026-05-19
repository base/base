//! `PolicyRegistry` native precompile — global singleton transfer-policy registry for B-20 tokens.

mod dispatch;

mod evm;
pub use evm::PolicyRegistryEvm;

mod policy;
pub use policy::Policy;

mod storage;
pub use storage::{POLICY_REGISTRY_ADDRESS, PolicyRegistryStorage, PolicyStorage};
