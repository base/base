use crate::{BlockEnv, CfgEnv};

/// Execution environment used by the Ethereum reference EVM in tests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReferenceEvmEnv {
    /// Ethereum configuration, including the reference fork identifier.
    pub cfg_env: CfgEnv,
    /// Block values supplied to the reference executor.
    pub block_env: BlockEnv,
}

impl ReferenceEvmEnv {
    /// Creates a reference environment from configuration and block values.
    pub const fn new(cfg_env: CfgEnv, block_env: BlockEnv) -> Self {
        Self { cfg_env, block_env }
    }
}
