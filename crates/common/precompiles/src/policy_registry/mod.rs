//! `PolicyRegistry` native precompile — global singleton transfer-policy registry for B-20 tokens.

mod abi;
pub use abi::IPolicyRegistry;

mod dispatch;

mod evm;
pub use evm::PolicyRegistryEvm;

mod policy;
pub use policy::PolicyHandle;

mod storage;
pub use storage::POLICY_REGISTRY_ADDRESS;
