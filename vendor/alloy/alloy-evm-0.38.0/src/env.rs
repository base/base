//! Configuration types for EVM environment.

use core::{any::Any, fmt::Debug};

use alloy_primitives::U256;
use revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{transaction::AccessList, TransactionType},
    primitives::hardfork::SpecId,
};

/// Container type that holds both the configuration and block environment for EVM execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmEnv<Spec = SpecId, BlockEnv = revm::context::BlockEnv> {
    /// The configuration environment with handler settings
    pub cfg_env: CfgEnv<Spec>,
    /// The block environment containing block-specific data
    pub block_env: BlockEnv,
}

impl<Spec: Default + Into<SpecId> + Clone, B: Default> Default for EvmEnv<Spec, B> {
    fn default() -> Self {
        Self { cfg_env: CfgEnv::new_with_spec(Spec::default()), block_env: B::default() }
    }
}

impl<Spec, BlockEnv> EvmEnv<Spec, BlockEnv> {
    /// Create a new `EvmEnv` from its components.
    ///
    /// # Arguments
    ///
    /// * `cfg_env_with_handler_cfg` - The configuration environment with handler settings
    /// * `block` - The block environment containing block-specific data
    pub const fn new(cfg_env: CfgEnv<Spec>, block_env: BlockEnv) -> Self {
        Self { cfg_env, block_env }
    }

    /// Configures the EVM execution limits.
    ///
    /// Sets `limit_contract_code_size`, `limit_contract_initcode_size`,
    /// and `tx_gas_limit_cap` from the provided [`EvmLimitParams`].
    pub const fn with_limits(mut self, limits: EvmLimitParams) -> Self {
        self.cfg_env.limit_contract_code_size = Some(limits.max_code_size);
        self.cfg_env.limit_contract_initcode_size = Some(limits.max_initcode_size);
        self.cfg_env.tx_gas_limit_cap = limits.tx_gas_limit_cap;
        self
    }
}

impl<Spec, BlockEnv: BlockEnvironment> EvmEnv<Spec, BlockEnv> {
    /// Sets an extension on the environment.
    pub fn map_block_env<NewBlockEnv>(
        self,
        f: impl FnOnce(BlockEnv) -> NewBlockEnv,
    ) -> EvmEnv<Spec, NewBlockEnv> {
        let Self { cfg_env, block_env } = self;
        EvmEnv { cfg_env, block_env: f(block_env) }
    }

    /// Returns a reference to the block environment.
    pub const fn block_env(&self) -> &BlockEnv {
        &self.block_env
    }

    /// Returns a reference to the configuration environment.
    pub const fn cfg_env(&self) -> &CfgEnv<Spec> {
        &self.cfg_env
    }

    /// Returns the chain ID of the environment.
    pub const fn chainid(&self) -> u64 {
        self.cfg_env.chain_id
    }

    /// Returns the spec id of the chain
    pub const fn spec_id(&self) -> &Spec {
        &self.cfg_env.spec
    }

    /// Overrides the configured block number
    pub fn with_block_number(mut self, number: U256) -> Self {
        self.block_env.inner_mut().number = number;
        self
    }

    /// Convenience function that overrides the configured block number with the given
    /// `Some(number)`.
    ///
    /// This is intended for block overrides.
    pub fn with_block_number_opt(mut self, number: Option<U256>) -> Self {
        if let Some(number) = number {
            self.block_env.inner_mut().number = number;
        }
        self
    }

    /// Sets the block number if provided.
    pub fn set_block_number_opt(&mut self, number: Option<U256>) -> &mut Self {
        if let Some(number) = number {
            self.block_env.inner_mut().number = number;
        }
        self
    }

    /// Overrides the configured block timestamp.
    pub fn with_timestamp(mut self, timestamp: U256) -> Self {
        self.block_env.inner_mut().timestamp = timestamp;
        self
    }

    /// Convenience function that overrides the configured block timestamp with the given
    /// `Some(timestamp)`.
    ///
    /// This is intended for block overrides.
    pub fn with_timestamp_opt(mut self, timestamp: Option<U256>) -> Self {
        if let Some(timestamp) = timestamp {
            self.block_env.inner_mut().timestamp = timestamp;
        }
        self
    }

    /// Sets the block timestamp if provided.
    pub fn set_timestamp_opt(&mut self, timestamp: Option<U256>) -> &mut Self {
        if let Some(timestamp) = timestamp {
            self.block_env.inner_mut().timestamp = timestamp;
        }
        self
    }

    /// Overrides the configured block base fee.
    pub fn with_base_fee(mut self, base_fee: u64) -> Self {
        self.block_env.inner_mut().basefee = base_fee;
        self
    }

    /// Convenience function that overrides the configured block base fee with the given
    /// `Some(base_fee)`.
    ///
    /// This is intended for block overrides.
    pub fn with_base_fee_opt(mut self, base_fee: Option<u64>) -> Self {
        if let Some(base_fee) = base_fee {
            self.block_env.inner_mut().basefee = base_fee;
        }
        self
    }

    /// Sets the block base fee if provided.
    pub fn set_base_fee_opt(&mut self, base_fee: Option<u64>) -> &mut Self {
        if let Some(base_fee) = base_fee {
            self.block_env.inner_mut().basefee = base_fee;
        }
        self
    }
}

impl<Spec, BlockEnv> From<(CfgEnv<Spec>, BlockEnv)> for EvmEnv<Spec, BlockEnv> {
    fn from((cfg_env, block_env): (CfgEnv<Spec>, BlockEnv)) -> Self {
        Self { cfg_env, block_env }
    }
}

/// Trait for types that can be used as a block environment.
///
/// Assumes that the type wraps an inner [`revm::context::BlockEnv`].
pub trait BlockEnvironment: revm::context::Block + Any + Debug + Send + Sync + 'static {
    /// Returns a mutable reference to the inner [`revm::context::BlockEnv`].
    fn inner_mut(&mut self) -> &mut revm::context::BlockEnv;
}

impl BlockEnvironment for BlockEnv {
    fn inner_mut(&mut self) -> &mut revm::context::BlockEnv {
        self
    }
}

/// Abstraction over mutable transaction environment.
///
/// Provides setters for common transaction fields, complementing
/// the read-only accessors on `revm::context::Transaction`.
pub trait TransactionEnvMut:
    revm::context::Transaction + Debug + Clone + Send + Sync + 'static
{
    /// Sets the gas limit.
    fn set_gas_limit(&mut self, gas_limit: u64);

    /// Sets the gas limit, returning `self`.
    fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.set_gas_limit(gas_limit);
        self
    }

    /// Sets the nonce.
    fn set_nonce(&mut self, nonce: u64);

    /// Sets the nonce, returning `self`.
    fn with_nonce(mut self, nonce: u64) -> Self {
        self.set_nonce(nonce);
        self
    }

    /// Sets the access list.
    fn set_access_list(&mut self, access_list: AccessList);

    /// Sets the access list, returning `self`.
    fn with_access_list(mut self, access_list: AccessList) -> Self {
        self.set_access_list(access_list);
        self
    }
}

impl TransactionEnvMut for TxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.gas_limit = gas_limit;
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.nonce = nonce;
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        self.access_list = access_list;

        if self.tx_type == TransactionType::Legacy as u8 {
            self.tx_type = TransactionType::Eip2930 as u8;
        }
    }
}

/// Parameters for EVM execution limits.
///
/// These parameters control configurable limits in the EVM that can be
/// overridden from their spec defaults (EIP-170, EIP-3860, EIP-7825).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvmLimitParams {
    /// Maximum bytecode size for deployed contracts.
    /// EIP-170 default: 24576 bytes (24KB)
    pub max_code_size: usize,
    /// Maximum initcode size for CREATE transactions.
    /// EIP-3860 default: 49152 bytes (48KB, 2x `max_code_size`)
    pub max_initcode_size: usize,
    /// Transaction gas limit cap.
    /// - `None` = use spec default (respects fork-aware defaults like EIP-7825)
    /// - `Some(cap)` = transactions with `gas_limit > cap` are rejected
    pub tx_gas_limit_cap: Option<u64>,
}

impl EvmLimitParams {
    /// Returns the Osaka EVM limit params.
    pub const fn osaka() -> Self {
        Self {
            max_code_size: revm::primitives::eip170::MAX_CODE_SIZE,
            max_initcode_size: revm::primitives::eip3860::MAX_INITCODE_SIZE,
            tx_gas_limit_cap: Some(revm::primitives::eip7825::TX_GAS_LIMIT_CAP),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::{Block, Cfg};

    #[test]
    fn test_evm_env_with_limits() {
        let limits = EvmLimitParams {
            max_code_size: 1234,
            max_initcode_size: 5678,
            tx_gas_limit_cap: Some(999_999),
        };

        let evm_env: EvmEnv<SpecId> = EvmEnv::default().with_limits(limits);

        assert_eq!(evm_env.cfg_env.max_code_size(), 1234);
        assert_eq!(evm_env.cfg_env.max_initcode_size(), 5678);
        assert_eq!(evm_env.cfg_env.tx_gas_limit_cap(), 999_999);
    }

    #[test]
    fn test_evm_env_with_osaka_defaults() {
        // osaka() provides explicit EIP-7825 gas cap and standard code size limits.
        let limits = EvmLimitParams::osaka();
        let evm_env: EvmEnv<SpecId> = EvmEnv::default().with_limits(limits);

        assert_eq!(evm_env.cfg_env.max_code_size(), revm::primitives::eip170::MAX_CODE_SIZE);
        assert_eq!(
            evm_env.cfg_env.max_initcode_size(),
            revm::primitives::eip3860::MAX_INITCODE_SIZE
        );
        assert_eq!(evm_env.cfg_env.tx_gas_limit_cap(), revm::primitives::eip7825::TX_GAS_LIMIT_CAP);
    }

    #[test]
    fn test_block_environment_is_dyn_compatible() {
        let block_env = BlockEnv::default();
        let dyn_block_env: &dyn BlockEnvironment = &block_env;

        assert_eq!(dyn_block_env.number(), block_env.number());
    }

    #[test]
    fn test_evm_env_with_osaka_limits() {
        // osaka() has tx_gas_limit_cap set to EIP-7825's cap.
        use revm::context::{BlockEnv, CfgEnv};

        let limits = EvmLimitParams::osaka();
        let cfg_env = CfgEnv::new_with_spec(SpecId::OSAKA);
        let evm_env = EvmEnv::new(cfg_env, BlockEnv::default()).with_limits(limits);

        assert_eq!(evm_env.cfg_env.tx_gas_limit_cap(), revm::primitives::eip7825::TX_GAS_LIMIT_CAP);
    }
}
